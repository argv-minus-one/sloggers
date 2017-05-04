use slog::Logger;

use Result;
use file::FileLoggerBuilder;
use null::NullLoggerBuilder;
use terminal::TerminalLoggerBuilder;

pub trait Build {
    fn build(&self) -> Result<Logger>;
}

#[derive(Debug)]
pub enum LoggerBuilder {
    File(FileLoggerBuilder),
    Null(NullLoggerBuilder),
    Terminal(TerminalLoggerBuilder),
}
impl Build for LoggerBuilder {
    fn build(&self) -> Result<Logger> {
        match *self {
            LoggerBuilder::File(ref b) => track!(b.build()),
            LoggerBuilder::Null(ref b) => track!(b.build()),
            LoggerBuilder::Terminal(ref b) => track!(b.build()),
        }
    }
}
