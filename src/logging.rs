use std::fs;
use std::path::Path;

use anyhow::Result;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::JsonFields;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

pub fn init_logging(log_file: &Path, level: &str, json: bool) -> Result<Vec<WorkerGuard>> {
    if let Some(parent) = log_file.parent() {
        fs::create_dir_all(parent)?;
    }
    let file_appender = tracing_appender::rolling::never(
        log_file.parent().unwrap_or_else(|| Path::new(".")),
        log_file.file_name().unwrap_or_default(),
    );
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));

    if json {
        tracing_subscriber::registry()
            .with(filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .fmt_fields(JsonFields::new())
                    .event_format(
                        tracing_subscriber::fmt::format()
                            .json()
                            .with_current_span(true)
                            .with_span_list(false),
                    )
                    .with_writer(std::io::stdout),
            )
            .with(
                tracing_subscriber::fmt::layer()
                    .fmt_fields(JsonFields::new())
                    .event_format(
                        tracing_subscriber::fmt::format()
                            .json()
                            .with_current_span(true)
                            .with_span_list(false),
                    )
                    .with_writer(file_writer),
            )
            .init();
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stdout))
            .with(tracing_subscriber::fmt::layer().with_writer(file_writer))
            .init();
    }

    Ok(vec![guard])
}
