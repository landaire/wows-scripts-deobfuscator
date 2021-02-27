
use anyhow::Result;

use cpython::{PyBytes, PyDict, PyList, PyModule, PyObject, PyResult, Python, PythonObject};
use log::{debug, trace};








use py_marshal::{Code};
use pydis::prelude::*;
use std::collections::HashMap;

use std::sync::Arc;

use crate::code_graph::*;

/// Deobfuscate the given code object. This will remove opaque predicates where possible,
/// simplify control flow to only go forward where possible, and rename local variables. This returns
/// the new bytecode and any function names resolved while deobfuscating the code object.
///
/// The returned HashMap is keyed by the code object's `$filename_$name` with a value of
/// what the suspected function name is.
pub fn deobfuscate_code(
    code: Arc<Code>,
    file_identifier: usize,
) -> Result<(Vec<u8>, HashMap<String, String>)> {
    let debug = !true;

    let _bytecode = code.code.as_slice();
    let _consts = Arc::clone(&code.consts);
    let mut new_bytecode: Vec<u8> = vec![];
    let mut mapped_function_names = HashMap::new();

    let mut code_graph = CodeGraph::from_code(Arc::clone(&code), file_identifier)?;

    code_graph.write_dot("before");

    code_graph.fix_bbs_with_bad_instr(code_graph.root, &code);

    code_graph.join_blocks();

    code_graph.write_dot("joined");

    code_graph.remove_const_conditions(&mut mapped_function_names);

    code_graph.write_dot("target");

    code_graph.join_blocks();

    code_graph.write_dot("joined");

    // update BB offsets
    //insert_jump_0(root_node_id, &mut code_graph);
    code_graph.update_bb_offsets();

    code_graph.write_dot("updated_bb");

    code_graph.massage_returns_for_decompiler();
    code_graph.update_bb_offsets();
    code_graph.update_branches();
    code_graph.update_bb_offsets();

    code_graph.write_dot("offsets");

    code_graph.write_bytecode(code_graph.root, &mut new_bytecode);

    if debug {
        let mut cursor = std::io::Cursor::new(&new_bytecode);
        trace!("{}", cursor.position());
        while let Ok(instr) = decode_py27(&mut cursor) {
            trace!("{:?}", instr);
            trace!("");
            trace!("{}", cursor.position());
        }
    }

    Ok((new_bytecode, mapped_function_names))
}

pub fn rename_vars<'a>(
    code_data: &[u8],
    deobfuscated_code: &'a mut impl Iterator<Item = &'a [u8]>,
    mapped_function_names: &HashMap<String, String>,
) -> PyResult<Vec<u8>> {
    let gil = Python::acquire_gil();

    let py = gil.python();

    let marshal = py.import("marshal")?;
    let types = py.import("types")?;

    let module = PyModule::new(py, "deob")?;
    module.add(py, "__builtins__", py.eval("__builtins__", None, None)?)?;

    module.add(py, "marshal", marshal)?;
    module.add(py, "types", types)?;
    module.add(py, "data", PyBytes::new(py, code_data))?;

    let converted_objects: Vec<PyObject> = deobfuscated_code
        .map(|code| PyBytes::new(py, code).into_object())
        .collect();

    module.add(
        py,
        "deobfuscated_code",
        PyList::new(py, converted_objects.as_slice()),
    )?;

    let mapped_names = PyDict::new(py);

    for (key, value) in mapped_function_names {
        mapped_names
            .set_item(
                py,
                cpython::PyString::new(py, key.as_ref()).into_object(),
                cpython::PyString::new(py, value.as_ref()).into_object(),
            )
            .expect("failed to set mapped function name");
    }
    module.add(py, "mapped_names", mapped_names)?;
    let locals = PyDict::new(py);
    locals.set_item(py, "deob", &module)?;

    let source = r#"
unknowns = 0

def cleanup_code_obj(code):
    global deobfuscated_code
    global mapped_names
    new_code = deobfuscated_code.pop(0)
    new_consts = []
    key = "{0}_{1}".format(code.co_filename, code.co_name)
    name = code.co_name
    if key in mapped_names:
        name = "{0}_{1}".format(mapped_names[key], name)
    else:
        name = fix_varnames([name])[0]
    filename = name
    for const in code.co_consts:
        if type(const) == types.CodeType:
            new_consts.append(cleanup_code_obj(const))
        else:
            new_consts.append(const)

    return types.CodeType(code.co_argcount, code.co_nlocals, code.co_stacksize, code.co_flags, new_code, tuple(new_consts), fix_varnames(code.co_names), fix_varnames(code.co_varnames), filename, name, code.co_firstlineno, code.co_lnotab, code.co_freevars, code.co_cellvars)


def fix_varnames(varnames):
    global unknowns
    newvars = []
    for var in varnames:
        var = var.strip()
        unallowed_chars = '=!@#$%^&*()"\'/,. '
        banned_char = False
        banned_words = ['assert', 'in', 'continue', 'break', 'for', 'def', 'as', 'elif', 'else', 'for', 'from', 'global', 'if', 'import', 'is', 'lambda', 'not', 'or', 'pass', 'print', 'return', 'while', 'with']
        for c in unallowed_chars:
            if c in var:
                banned_char = True

        if not banned_char:
            if var in banned_words:
                banned_char = True

        if banned_char:
            newvars.append('unknown_{0}'.format(unknowns))
            unknowns += 1
        else:
            newvars.append(var)
    
    return tuple(newvars)


code = marshal.loads(data)
output = marshal.dumps(cleanup_code_obj(code))
"#;

    locals.set_item(py, "source", source)?;

    let output = py.run("exec source in deob.__dict__", None, Some(&locals))?;
    debug!("{:?}", output);

    let output = module
        .get(py, "output")?
        .cast_as::<PyBytes>(py)?
        .data(py)
        .to_vec();

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_graph::tests::*;
    use crate::smallvm::tests::*;
    use crate::smallvm::PYTHON27_COMPARE_OPS;
    use crate::{Instr, Long};
    use num_bigint::BigInt;
    use pydis::opcode::Instruction;

    type TargetOpcode = pydis::opcode::Python27;

    #[test]
    fn simple_deobfuscation() {
        simple_logger::SimpleLogger::new()
            .with_level(log::LevelFilter::Trace)
            .init()
            .unwrap();

        let mut code = default_code_obj();

        let consts = vec![Obj::None, Long!(1), Long!(2)];

        Arc::get_mut(&mut code).unwrap().consts = Arc::new(consts);

        let instrs = [
            // 0
            Instr!(TargetOpcode::JUMP_ABSOLUTE, 3),
            // 3
            Instr!(TargetOpcode::JUMP_ABSOLUTE, 6),
            // 6
            Instr!(TargetOpcode::LOAD_CONST, 1),
            // 9
            Instr!(TargetOpcode::LOAD_CONST, 2),
            // 12. 1 < 2, should evaluate to true
            Instr!(
                TargetOpcode::COMPARE_OP,
                PYTHON27_COMPARE_OPS
                    .iter()
                    .position(|op| *op == "<")
                    .unwrap() as u16
            ),
            // 15
            Instr!(TargetOpcode::POP_JUMP_IF_TRUE, 22), // jump to target 1
            // 18
            Instr!(TargetOpcode::LOAD_CONST, 0),
            // 21
            Instr!(TargetOpcode::RETURN_VALUE),
            // 22
            Instr!(TargetOpcode::LOAD_CONST, 1), // target 1
            // 25
            Instr!(TargetOpcode::RETURN_VALUE),
        ];

        let expected = [
            Instr!(TargetOpcode::LOAD_CONST, 1),
            Instr!(TargetOpcode::RETURN_VALUE),
        ];

        change_code_instrs(&mut code, &instrs[..]);

        let (new_bytecode, _mapped_names) = deobfuscate_code(Arc::clone(&code), 0).unwrap();

        // We now need to change this back into a graph for ease of testing
        let mut expected_bytecode = vec![];
        for instr in &expected {
            serialize_instr(instr, &mut expected_bytecode);
        }

        assert_eq!(new_bytecode, expected_bytecode);
    }
}
