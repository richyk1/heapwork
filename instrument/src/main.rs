//! Adds safepoints to a compiled `.wasm`, so code that never yields can be preempted.
//!
//! - every `loop` header gets: `if (--counter == 0) { counter = CHECK_EVERY; env.safepoint() }`,
//!   a poll every 10,000 iterations that lets the host interrupt any loop;
//! - every mutable global (e.g. LLVM's `__stack_pointer`) is exported, so the host can save and
//!   restore it: a snapshot is linear memory plus these globals.
//!
//! Asyncify (`wasm-opt --asyncify`) runs afterwards and makes `env.safepoint` / `env.llm` able to
//! unwind the whole call stack into linear memory.
//!
//! usage: instrument <in.wasm> <out.wasm>

use walrus::{
    ir::{BinaryOp, Instr, InstrSeqId, UnaryOp, Value},
    ConstExpr, ExportItem, FunctionId, GlobalId, GlobalKind, LocalFunction, Module, ValType,
};

const CHECK_EVERY: i32 = 10_000;

/// Every `loop` body reachable from `seq`.
fn collect_loops(func: &LocalFunction, seq: InstrSeqId, out: &mut Vec<InstrSeqId>) {
    for (instr, _) in &func.block(seq).instrs {
        match instr {
            Instr::Block(b) => collect_loops(func, b.seq, out),
            Instr::Loop(l) => {
                out.push(l.seq);
                collect_loops(func, l.seq, out);
            }
            Instr::IfElse(ie) => {
                collect_loops(func, ie.consequent, out);
                collect_loops(func, ie.alternative, out);
            }
            _ => {}
        }
    }
}

fn add_safepoint(func: &mut LocalFunction, body: InstrSeqId, counter: GlobalId, safepoint: FunctionId) {
    let builder = func.builder_mut();
    let then = {
        let mut seq = builder.dangling_instr_seq(None);
        seq.i32_const(CHECK_EVERY).global_set(counter).call(safepoint);
        seq.id()
    };
    let otherwise = builder.dangling_instr_seq(None).id();
    let check: [Instr; 7] = [
        Instr::GlobalGet(walrus::ir::GlobalGet { global: counter }),
        Instr::Const(walrus::ir::Const { value: Value::I32(1) }),
        Instr::Binop(walrus::ir::Binop { op: BinaryOp::I32Sub }),
        Instr::GlobalSet(walrus::ir::GlobalSet { global: counter }),
        Instr::GlobalGet(walrus::ir::GlobalGet { global: counter }),
        Instr::Unop(walrus::ir::Unop { op: UnaryOp::I32Eqz }),
        Instr::IfElse(walrus::ir::IfElse { consequent: then, alternative: otherwise }),
    ];
    let mut seq = builder.instr_seq(body);
    for (pos, instr) in check.into_iter().enumerate() {
        seq.instr_at(pos, instr);
    }
}

fn main() -> walrus::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [input, output] = args.as_slice() else {
        eprintln!("usage: instrument <in.wasm> <out.wasm>");
        std::process::exit(2);
    };
    let mut module = Module::from_file(input)?;

    let ty = module.types.add(&[], &[]);
    let (safepoint, _) = module.add_import_func("env", "safepoint", ty);
    let counter = module
        .globals
        .add_local(ValType::I32, true, false, ConstExpr::Value(Value::I32(CHECK_EVERY)));

    let mut instrumented = 0;
    for (_, func) in module.funcs.iter_local_mut() {
        let mut loops = Vec::new();
        collect_loops(func, func.entry_block(), &mut loops);
        for body in loops {
            add_safepoint(func, body, counter, safepoint);
            instrumented += 1;
        }
    }

    let exported: Vec<GlobalId> = module
        .exports
        .iter()
        .filter_map(|e| match e.item {
            ExportItem::Global(g) => Some(g),
            _ => None,
        })
        .collect();
    let to_export: Vec<GlobalId> = module
        .globals
        .iter()
        .filter(|g| g.mutable && matches!(g.kind, GlobalKind::Local(_)) && !exported.contains(&g.id()))
        .map(|g| g.id())
        .collect();
    for (n, g) in to_export.iter().enumerate() {
        module.exports.add(&format!("heapwork_global_{n}"), *g);
    }

    module.emit_wasm_file(output)?;
    eprintln!("instrument: {instrumented} loops got a safepoint, {} globals exported", to_export.len());
    Ok(())
}
