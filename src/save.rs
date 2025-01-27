use std::{
    io::{ErrorKind, Read},
    path::PathBuf,
    str::FromStr,
    sync::{atomic::AtomicBool, Arc},
};

use bytes::Bytes;
use log::debug;
use nu_plugin::{EngineInterface, EvaluatedCall, PluginCommand};
use nu_protocol::{
    process::ChildPipe, ByteStreamSource, Category, IntoSpanned, LabeledError, ListStream,
    PipelineData, ShellError, Signals, Signature, Span, Spanned, SyntaxShape, Type, Value,
};
use object_store::{PutPayload, WriteMultipart};
use url::Url;

use crate::CloudPlugin;

pub struct Save;

impl PluginCommand for Save {
    type Plugin = CloudPlugin;

    fn name(&self) -> &str {
        "cloud save"
    }

    fn signature(&self) -> nu_protocol::Signature {
        Signature::build("cloud save")
            .input_output_types(vec![(Type::Any, Type::Nothing)])
            .required("uri", SyntaxShape::String, "The file url to use.")
            .switch("raw", "save file as raw binary", Some('r'))
            .category(Category::FileSystem)
    }

    fn usage(&self) -> &str {
        "Save pipeline inptu "
    }

    fn run(
        &self,
        plugin: &Self::Plugin,
        engine: &EngineInterface,
        call: &nu_plugin::EvaluatedCall,
        input: PipelineData,
    ) -> Result<PipelineData, LabeledError> {
        command(plugin, engine, call, input).map_err(LabeledError::from)
    }
}

fn command(
    plugin: &CloudPlugin,
    engine: &EngineInterface,
    call: &nu_plugin::EvaluatedCall,
    input: PipelineData,
) -> Result<PipelineData, ShellError> {
    let raw = call.has_flag("raw")?;
    let call_span = call.head;
    let url_path: Spanned<PathBuf> = call.req(0)?;
    let url = url_path
        .item
        .to_str()
        .expect("The path should already be unicode")
        .to_string();
    let url = Spanned {
        item: Url::from_str(&url).map_err(|e| ShellError::IncorrectValue {
            msg: format!("Invalid Url: {e}"),
            val_span: url_path.span,
            call_span,
        })?,
        span: url_path.span,
    };

    match input {
        PipelineData::ByteStream(stream, _metadata) => {
            debug!("Handling byte stream");
            // todo - fix when 0.97 is out
            let signals = Signals::new(Arc::new(AtomicBool::new(false)));

            match stream.into_source() {
                ByteStreamSource::Read(read) => {
                    bytestream_to_cloud(plugin, read, &signals, &url, call_span)?;
                }
                ByteStreamSource::File(source) => {
                    bytestream_to_cloud(plugin, source, &signals, &url, call_span)?;
                }
                ByteStreamSource::Child(mut child) => {
                    match child.stdout.take() {
                        Some(stdout) => {
                            let res = match stdout {
                                ChildPipe::Pipe(pipe) => {
                                    bytestream_to_cloud(plugin, pipe, &signals, &url, call_span)
                                }
                                ChildPipe::Tee(tee) => {
                                    bytestream_to_cloud(plugin, tee, &signals, &url, call_span)
                                }
                            };
                            res?;
                        }
                        _ => {}
                    };
                }
            }

            Ok(PipelineData::Empty)
        }
        PipelineData::ListStream(ls, _pipeline_metadata) if raw => {
            debug!("Handling list stream");
            // todo - update the signals stuff when it is available for plugins 0.97
            plugin
                .rt
                .block_on(liststream_to_cloud(ls, &Signals::empty(), &url, call_span))?;
            Ok(PipelineData::empty())
        }
        input => {
            debug!("Handling input");
            let bytes = input_to_bytes(input, &url_path.item, raw, engine, call, call_span)?;

            plugin.rt.block_on(stream_bytes(bytes, &url, call_span))?;

            Ok(PipelineData::empty())
        }
    }
}

async fn liststream_to_cloud(
    ls: ListStream,
    signals: &Signals,
    url: &Spanned<Url>,
    span: Span,
) -> Result<(), ShellError> {
    let (object_store, path) = crate::parse_url(url, span).await?;
    let upload = object_store.put_multipart(&path).await.unwrap();
    let mut write = WriteMultipart::new(upload);

    for v in ls {
        signals.check(span)?;
        let bytes = value_to_bytes(v)?;
        write.write(&bytes)
    }

    let _ = write.finish().await.map_err(|e| ShellError::GenericError {
        error: format!("Could not write to S3: {e}"),
        msg: "".into(),
        span: None,
        help: None,
        inner: vec![],
    })?;

    Ok(())
}

fn bytestream_to_cloud(
    plugin: &CloudPlugin,
    source: impl Read,
    signals: &Signals,
    url: &Spanned<Url>,
    span: Span,
) -> Result<(), ShellError> {
    plugin
        .rt
        .block_on(stream_to_cloud_async(source, signals, url, span))
}

async fn stream_to_cloud_async(
    source: impl Read,
    signals: &Signals,
    url: &Spanned<Url>,
    span: Span,
) -> Result<(), ShellError> {
    let (object_store, path) = crate::parse_url(url, span).await?;
    let upload = object_store.put_multipart(&path).await.unwrap();
    let mut write = WriteMultipart::new(upload);

    let _ = generic_copy(source, &mut write, span, signals)?;

    let _ = write.finish().await.map_err(|e| ShellError::GenericError {
        error: format!("Could not write to S3: {e}"),
        msg: "".into(),
        span: None,
        help: None,
        inner: vec![],
    })?;

    Ok(())
}

const DEFAULT_BUF_SIZE: usize = 8192;

// Copied from [`std::io::copy`]
fn generic_copy(
    mut reader: impl Read,
    writer: &mut WriteMultipart,
    span: Span,
    signals: &Signals,
) -> Result<u64, ShellError> {
    let buf = &mut [0; DEFAULT_BUF_SIZE];
    let mut len = 0;
    loop {
        signals.check(span)?;
        let n = match reader.read(buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into_spanned(span).into()),
        };
        len += n;
        writer.write(&buf[..n]);
    }
    Ok(len as u64)
}

/// Convert [`Value::String`] [`Value::Binary`] or [`Value::List`] into [`Vec`] of bytes
///
/// Propagates [`Value::Error`] and creates error otherwise
fn value_to_bytes(value: Value) -> Result<Vec<u8>, ShellError> {
    match value {
        Value::String { val, .. } => Ok(val.into_bytes()),
        Value::Binary { val, .. } => Ok(val),
        Value::List { vals, .. } => {
            let val = vals
                .into_iter()
                .map(Value::coerce_into_string)
                .collect::<Result<Vec<String>, ShellError>>()?
                .join("\n")
                + "\n";

            Ok(val.into_bytes())
        }
        // Propagate errors by explicitly matching them before the final case.
        Value::Error { error, .. } => Err(*error),
        other => Ok(other.coerce_into_string()?.into_bytes()),
    }
}

/// Convert [`PipelineData`] bytes to write in file, possibly converting
/// to format of output file
fn input_to_bytes(
    input: PipelineData,
    path: &std::path::Path,
    raw: bool,
    engine: &EngineInterface,
    call: &EvaluatedCall,
    span: Span,
) -> Result<Vec<u8>, ShellError> {
    let ext = if raw {
        None
    } else if let PipelineData::ByteStream(..) = input {
        None
    } else if let PipelineData::Value(Value::String { .. }, ..) = input {
        None
    } else {
        path.extension()
            .map(|name| name.to_string_lossy().to_string())
    };

    let input = if let Some(ext) = ext {
        convert_to_extension(engine, &ext, input, call)?
    } else {
        input
    };

    value_to_bytes(input.into_value(span)?)
}

/// Convert given data into content of file of specified extension if
/// corresponding `to` command exists. Otherwise attempt to convert
/// data to bytes as is
fn convert_to_extension(
    engine: &EngineInterface,
    extension: &str,
    input: PipelineData,
    call: &EvaluatedCall,
) -> Result<PipelineData, ShellError> {
    if let Some(decl_id) = engine.find_decl(format!("to {extension}"))? {
        debug!("Found to {extension} decl: converting input");
        let command_output = engine.call_decl(decl_id, call.clone(), input, true, false)?;
        Ok(command_output)
    } else {
        Ok(input)
    }
}

async fn stream_bytes(bytes: Vec<u8>, url: &Spanned<Url>, span: Span) -> Result<(), ShellError> {
    let (object_store, path) = crate::parse_url(url, span).await?;

    let payload = PutPayload::from_bytes(Bytes::from(bytes));
    object_store
        .put(&path, payload)
        .await
        .map_err(|e| ShellError::GenericError {
            error: format!("Could not write to S3: {e}"),
            msg: "".into(),
            span: None,
            help: None,
            inner: vec![],
        })?;

    Ok(())
}
