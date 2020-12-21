use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("unexpected data type while processing `{0}`: {1:?}")]
    ObjectError(&'static str, py_marshal::Obj),
    #[error("error disassembling bytecode: {0}")]
    DisassemblerError(#[from] pydis::error::DecodeError),
}

#[derive(Error, Debug)]
pub enum ExecutionError {
    #[error("complex opcode/object type encountered. Opcode: {0:?}, Object Type: {1:?}")]
    ComplexExpression(
        pydis::opcode::Instruction<pydis::opcode::Python27>,
        Option<py_marshal::Type>,
    ),

    #[error("unsupported instruction encountered: {0:?}")]
    UnsupportedOpcode(pydis::opcode::Python27),
}
