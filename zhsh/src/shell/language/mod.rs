//! Native 命令语言入口：读取和解析输入，不执行命令、不访问会话状态或文件系统。
mod simple;
pub(crate) use simple::{
    parse_single, read_unit, InputError, ReadResult, SimpleReader, SimpleUnit,
};
