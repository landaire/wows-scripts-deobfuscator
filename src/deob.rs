use crate::smallvm::ParsedInstr;
use anyhow::Result;
use bitflags::bitflags;
use cpython::{PyBytes, PyDict, PyList, PyModule, PyObject, PyResult, Python, PythonObject};
use log::{debug, trace};
use num_bigint::ToBigInt;
use petgraph::algo::astar;
use petgraph::algo::dijkstra;
use petgraph::graph::{Graph, NodeIndex};
use petgraph::visit::{Bfs, EdgeRef};
use petgraph::Direction;
use petgraph::IntoWeightedEdge;
use py_marshal::{Code, Obj};
use pydis::prelude::*;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use once_cell::sync::OnceCell;

use crate::code_graph::*;

pub(crate) static FILES_PROCESSED: OnceCell<AtomicUsize> = OnceCell::new();

/// Deobfuscate the given code object. This will remove opaque predicates where possible,
/// simplify control flow to only go forward where possible, and rename local variables. This returns
/// the new bytecode and any function names resolved while deobfuscating the code object.
///
/// The returned HashMap is keyed by the code object's `$filename_$name` with a value of
/// what the suspected function name is.
pub fn deobfuscate_code(code: Arc<Code>) -> Result<(Vec<u8>, HashMap<String, String>)> {
    let debug = !true;

    let _bytecode = code.code.as_slice();
    let _consts = Arc::clone(&code.consts);
    let mut new_bytecode: Vec<u8> = vec![];
    let mut mapped_function_names = HashMap::new();

    let mut code_graph= CodeGraph::from_code(Arc::clone(&code))?;

    // Start joining blocks
    let mut counter = 0;
    for i in 0..200 {
        if !std::path::PathBuf::from(format!("before_{}.dot", i)).exists() {
            counter = i;
            break;
        }
    }

    code_graph.write_dot("before");

    code_graph.fix_bbs_with_bad_instr(code_graph.root, &code);

    // if first.opcode == TargetOpcode::JUMP_ABSOLUTE && first.arg.unwrap() == 44 {
    //     panic!("");
    // }
    while code_graph.join_blocks(code_graph.root) {}

    code_graph.write_dot("joined");

    code_graph.remove_const_conditions(&mut mapped_function_names);

    code_graph.write_dot("target");

    while code_graph.join_blocks(code_graph.root) {}

    code_graph.write_dot("joined");

    // update BB offsets
    //insert_jump_0(root_node_id, &mut code_graph);
    code_graph.update_bb_offsets(code_graph.root);

    code_graph.write_dot("updated_bb");

    if code_graph.update_branches(code_graph.root) {
        code_graph.clear_flags(
            code_graph.root,
            BasicBlockFlags::OFFSETS_UPDATED,
        );
        code_graph.update_bb_offsets(code_graph.root);
    }
    code_graph.clear_flags(
        code_graph.root,
        BasicBlockFlags::OFFSETS_UPDATED,
    );
    code_graph.update_bb_offsets(code_graph.root);

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

    FILES_PROCESSED.get().unwrap().fetch_add(1, Ordering::Relaxed);

    Ok((new_bytecode, mapped_function_names))
}


pub fn rename_vars(
    code_data: &[u8],
    deobfuscated_code: &[Vec<u8>],
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
        .iter()
        .map(|code| PyBytes::new(py, code.as_slice()).into_object())
        .collect();

    module.add(
        py,
        "deobfuscated_code",
        PyList::new(py, converted_objects.as_slice()),
    )?;

    let mapped_names = PyDict::new(py);

    for (key, value) in mapped_function_names {
        mapped_names.set_item(
            py,
            cpython::PyString::new(py, key.as_ref()).into_object(),
            cpython::PyString::new(py, value.as_ref()).into_object(),
        );
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
    for const in code.co_consts:
        if type(const) == types.CodeType:
            new_consts.append(cleanup_code_obj(const))
        else:
            new_consts.append(const)

    return types.CodeType(code.co_argcount, code.co_nlocals, code.co_stacksize, code.co_flags, new_code, tuple(new_consts), fix_varnames(code.co_names), fix_varnames(code.co_varnames), code.co_filename, name, code.co_firstlineno, code.co_lnotab, code.co_freevars, code.co_cellvars)


def fix_varnames(varnames):
    global unknowns
    newvars = []
    for var in varnames:
        var = var.strip()
        unallowed_chars = '!@#$%^&*()"\'/,. '
        banned_char = False
        for c in unallowed_chars:
            if c in var:
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
