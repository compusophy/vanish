//! corpus capture (CARTRIDGE_PLAN §9, build item 9): every candidate
//! cartridge, its opcode trace, and the runtime's verdict on it.
//!
//! WHY THIS EXISTS, and why it is not optional. §9's horizon is a model
//! whose output space is opcodes for this VM rather than source text, with
//! the runtime as the reward model — "do not start this until the corpus
//! collection exists, or there is nothing to train on". this module is that
//! collection. it is also the only thing in the substrate that can answer a
//! question item 8d could not: a policy can be verified to DO what it says,
//! but nothing yet records what was TRIED and what happened.
//!
//! WHAT IS RECORDED, and what is not. the unit is a candidate program: its
//! rustlite source, its opcode trace, where it came from, and the verifier's
//! verdict — verified or refused, with the refusal's own words. REFUSALS ARE
//! KEPT. a corpus of only successes teaches nothing about the boundary, and
//! the boundary is where a generated program actually fails.
//!
//! the trace drops operands. that is a deliberate loss and it costs nothing,
//! because emission is deterministic: `emit_module(parse(source))` rebuilds
//! the exact module at any time, so the source IS the record and the trace
//! is the same program in the shape a model would emit it. storing operands
//! would double the corpus to store what is already there.

use std::collections::BTreeMap;

use super::runtime::{decode, BinOp, ExportKind, Instr};

/// on-disk format. a corpus written by a newer build is refused rather than
/// half-read, the same rule the cartridge kv follows.
pub const CORPUS_VERSION: u64 = 1;

/// how many programs are kept. a bounded log, like the transcript: the
/// newest samples are the ones a training run or a reader wants, and an
/// unbounded one would grow until opfs refused a write.
pub const MAX_SAMPLES: usize = 200;

/// bytes of source kept per sample. a policy is a small module; anything
/// bigger than this is not what this corpus is for.
pub const MAX_SOURCE: usize = 16_384;

/// where a candidate came from. the `prompt` side of §9's (prompt → trace)
/// pair is `Agent { intent }`: the model's own statement of what it meant
/// the program to do, which is the only prompt-shaped thing the swap path
/// ever sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// pasted into the ui by a person.
    Human,
    /// written by the agent through the `swap_cartridge` tool.
    Agent { intent: String },
}

impl Origin {
    pub fn tag(&self) -> &'static str {
        match self {
            Origin::Human => "human",
            Origin::Agent { .. } => "agent",
        }
    }

    pub fn intent(&self) -> &str {
        match self {
            Origin::Human => "",
            Origin::Agent { intent } => intent,
        }
    }
}

/// what the runtime decided. the refusal text is the compiler's or the
/// rehearsal's own words, never a summary — a label a future trainer can
/// group on only if it is the real one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// compiled, instantiated, and survived a rehearsal of both phases.
    Verified {
        /// what the rehearsal probe became under this program.
        shaped: String,
    },
    Refused {
        reason: String,
    },
}

impl Verdict {
    pub fn is_verified(&self) -> bool {
        matches!(self, Verdict::Verified { .. })
    }
}

/// one function's opcode trace. `name` is the export name, or `fn#N` for an
/// internal function — the emitter does not name those and the wasm format
/// does not carry them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FnOps {
    pub name: String,
    pub ops: Vec<String>,
}

/// one captured program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    /// fingerprint of the source: the identity, so re-trying the same
    /// program updates its verdict instead of growing the corpus.
    pub id: String,
    /// epoch millis, PASSED IN — this module reads no clock (D1).
    pub at_ms: i64,
    pub origin: Origin,
    pub source: String,
    /// empty when the source did not compile: there is no trace of a
    /// program that was never emitted, and pretending otherwise would put a
    /// fabricated label in the training data.
    pub ops: Vec<FnOps>,
    pub verdict: Verdict,
}

impl Sample {
    /// total instructions across every function — the cheapest size signal,
    /// and the one that matters for a model with a token budget.
    pub fn op_count(&self) -> usize {
        self.ops.iter().map(|f| f.ops.len()).sum()
    }
}

/// the bounded log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Corpus {
    /// oldest first; the newest sample is last.
    pub samples: Vec<Sample>,
}

impl Corpus {
    pub fn new() -> Self {
        Self::default()
    }

    /// add a sample, or update the one with the same fingerprint IN PLACE.
    ///
    /// re-recording the same source is not new evidence, but the verdict can
    /// legitimately change — the same program refused against one store and
    /// verified against another is exactly the kind of thing a reader needs
    /// to see. returns true when this was a program the corpus had not seen.
    pub fn record(&mut self, sample: Sample) -> bool {
        if let Some(existing) = self.samples.iter_mut().find(|s| s.id == sample.id) {
            *existing = sample;
            return false;
        }
        self.samples.push(sample);
        if self.samples.len() > MAX_SAMPLES {
            let drop = self.samples.len() - MAX_SAMPLES;
            self.samples.drain(0..drop);
        }
        true
    }

    pub fn verified(&self) -> usize {
        self.samples.iter().filter(|s| s.verdict.is_verified()).count()
    }

    pub fn refused(&self) -> usize {
        self.samples.len() - self.verified()
    }

    /// how often each opcode appears across VERIFIED programs. this is the
    /// vocabulary §9 wants: the instruction selection rustlite actually
    /// makes, measured rather than assumed.
    pub fn histogram(&self) -> BTreeMap<String, usize> {
        let mut out = BTreeMap::new();
        for sample in self.samples.iter().filter(|s| s.verdict.is_verified()) {
            for f in &sample.ops {
                for op in &f.ops {
                    *out.entry(op.clone()).or_insert(0) += 1;
                }
            }
        }
        out
    }

    /// the n most common opcodes, most frequent first — a one-line summary
    /// for a feed note or a tool result.
    pub fn top_ops(&self, n: usize) -> Vec<(String, usize)> {
        let mut all: Vec<(String, usize)> = self.histogram().into_iter().collect();
        // count descending, then name, so the summary is stable.
        all.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        all.truncate(n);
        all
    }

    pub fn encode(&self) -> String {
        let rows: Vec<serde_json::Value> = self
            .samples
            .iter()
            .map(|s| {
                serde_json::json!({
                    "id": s.id,
                    "at_ms": s.at_ms,
                    "origin": s.origin.tag(),
                    "intent": s.origin.intent(),
                    "source": s.source,
                    "ops": s.ops.iter().map(|f| serde_json::json!({
                        "name": f.name,
                        "ops": f.ops,
                    })).collect::<Vec<_>>(),
                    "verdict": match &s.verdict {
                        Verdict::Verified { shaped } => serde_json::json!({
                            "verified": true, "shaped": shaped,
                        }),
                        Verdict::Refused { reason } => serde_json::json!({
                            "verified": false, "reason": reason,
                        }),
                    },
                })
            })
            .collect();
        serde_json::json!({ "version": CORPUS_VERSION, "samples": rows }).to_string()
    }

    pub fn decode(text: &str) -> Result<Self, String> {
        let doc: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("corpus is not json: {e}"))?;
        match doc.get("version").and_then(|v| v.as_u64()) {
            Some(CORPUS_VERSION) => {}
            Some(other) => {
                return Err(format!(
                    "corpus is format version {other}; this build reads {CORPUS_VERSION}"
                ))
            }
            None => return Err("corpus has no `version`".to_string()),
        }
        let rows = doc
            .get("samples")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "corpus has no `samples` array".to_string())?;

        let mut samples = Vec::with_capacity(rows.len());
        for (i, row) in rows.iter().enumerate() {
            let str_at = |k: &str| -> String {
                row.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string()
            };
            let id = row
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("corpus sample {i} has no `id`"))?
                .to_string();
            let origin = match row.get("origin").and_then(|v| v.as_str()) {
                Some("agent") => Origin::Agent {
                    intent: str_at("intent"),
                },
                Some("human") => Origin::Human,
                other => {
                    return Err(format!(
                        "corpus sample {i} has an unknown origin {other:?}"
                    ))
                }
            };
            let verdict = row
                .get("verdict")
                .ok_or_else(|| format!("corpus sample {i} has no `verdict`"))?;
            let verdict = if verdict.get("verified").and_then(|v| v.as_bool()) == Some(true) {
                Verdict::Verified {
                    shaped: verdict
                        .get("shaped")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                }
            } else {
                Verdict::Refused {
                    reason: verdict
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                }
            };
            let ops = row
                .get("ops")
                .and_then(|v| v.as_array())
                .map(|list| {
                    list.iter()
                        .map(|f| FnOps {
                            name: f
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            ops: f
                                .get("ops")
                                .and_then(|v| v.as_array())
                                .map(|o| {
                                    o.iter()
                                        .filter_map(|x| x.as_str().map(str::to_string))
                                        .collect()
                                })
                                .unwrap_or_default(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            samples.push(Sample {
                id,
                at_ms: row.get("at_ms").and_then(|v| v.as_i64()).unwrap_or(0),
                origin,
                source: str_at("source"),
                ops,
                verdict,
            });
        }
        Ok(Corpus { samples })
    }
}

/// build the sample for one candidate. the caller has already decided the
/// verdict; this turns (source, module, verdict) into the record.
///
/// `module` is None exactly when there is nothing to trace — the source did
/// not compile — and the sample then carries an empty `ops`.
pub fn sample(
    source: &str,
    module: Option<&[u8]>,
    origin: Origin,
    verdict: Verdict,
    at_ms: i64,
) -> Sample {
    let ops = module.and_then(|m| opcodes(m).ok()).unwrap_or_default();
    let mut kept = source.to_string();
    kept.truncate(clamp_char_boundary(&kept, MAX_SOURCE));
    Sample {
        id: fingerprint(source),
        at_ms,
        origin,
        source: kept,
        ops,
        verdict,
    }
}

/// truncating a String at a byte index panics mid-character; back up to the
/// nearest boundary at or below the cap.
fn clamp_char_boundary(s: &str, cap: usize) -> usize {
    if s.len() <= cap {
        return s.len();
    }
    let mut i = cap;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// the opcode trace of an emitted module, one entry per DEFINED function
/// (imports have no body). exported functions carry their export name.
pub fn opcodes(module_bytes: &[u8]) -> Result<Vec<FnOps>, String> {
    let m = decode(module_bytes).map_err(|e| format!("{e:?}"))?;
    // the wasm index space puts imports first, so a defined function's
    // index is imports.len() + its position in `funcs`.
    let base = m.imports.len();
    let mut names: BTreeMap<usize, String> = BTreeMap::new();
    for e in &m.exports {
        if e.kind == ExportKind::Func {
            names.insert(e.index as usize, e.name.clone());
        }
    }
    Ok(m.funcs
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let idx = base + i;
            FnOps {
                name: names
                    .get(&idx)
                    .cloned()
                    .unwrap_or_else(|| format!("fn#{idx}")),
                ops: f.code.iter().map(|instr| mnemonic(instr).to_string()).collect(),
            }
        })
        .collect())
}

/// one stable token per instruction. OPERANDS ARE DROPPED — see the module
/// header: the source rebuilds them exactly, so storing them again would
/// double the corpus to record what it already holds.
pub fn mnemonic(i: &Instr) -> &'static str {
    match i {
        Instr::I32Const(_) => "i32.const",
        Instr::I64Const(_) => "i64.const",
        Instr::F32Const(_) => "f32.const",
        Instr::F64Const(_) => "f64.const",
        Instr::LocalGet(_) => "local.get",
        Instr::LocalSet(_) => "local.set",
        Instr::Call(_) => "call",
        Instr::Drop => "drop",
        Instr::Unreachable => "unreachable",
        Instr::Br(_) => "br",
        Instr::BrIf(_) => "br_if",
        Instr::I32Eqz => "i32.eqz",
        Instr::I64ExtendI32U => "i64.extend_i32_u",
        Instr::I32WrapI64 => "i32.wrap_i64",
        Instr::I32Load(_) => "i32.load",
        Instr::I32Load8U(_) => "i32.load8_u",
        Instr::I32Store(_) => "i32.store",
        Instr::I32Store8(_) => "i32.store8",
        Instr::MemorySize => "memory.size",
        Instr::FunctionEnd => "end",
        Instr::Bin(op) => bin_mnemonic(*op),
    }
}

fn bin_mnemonic(op: BinOp) -> &'static str {
    match op {
        BinOp::I32Add => "i32.add",
        BinOp::I32Sub => "i32.sub",
        BinOp::I32Mul => "i32.mul",
        BinOp::I32DivS => "i32.div_s",
        BinOp::I32RemS => "i32.rem_s",
        BinOp::I32LtS => "i32.lt_s",
        BinOp::I32GtS => "i32.gt_s",
        BinOp::I32LeS => "i32.le_s",
        BinOp::I32GeS => "i32.ge_s",
        BinOp::I32Eq => "i32.eq",
        BinOp::I32Ne => "i32.ne",
        BinOp::I64Add => "i64.add",
        BinOp::I64Sub => "i64.sub",
        BinOp::I64Mul => "i64.mul",
        BinOp::I64DivS => "i64.div_s",
        BinOp::I64RemS => "i64.rem_s",
        BinOp::I64LtS => "i64.lt_s",
        BinOp::I64GtS => "i64.gt_s",
        BinOp::I64LeS => "i64.le_s",
        BinOp::I64GeS => "i64.ge_s",
        BinOp::I64Eq => "i64.eq",
        BinOp::I64Ne => "i64.ne",
        BinOp::F32Add => "f32.add",
        BinOp::F32Sub => "f32.sub",
        BinOp::F32Mul => "f32.mul",
        BinOp::F32Div => "f32.div",
        BinOp::F32Eq => "f32.eq",
        BinOp::F32Ne => "f32.ne",
        BinOp::F32Lt => "f32.lt",
        BinOp::F32Gt => "f32.gt",
        BinOp::F32Le => "f32.le",
        BinOp::F32Ge => "f32.ge",
        BinOp::F64Add => "f64.add",
        BinOp::F64Sub => "f64.sub",
        BinOp::F64Mul => "f64.mul",
        BinOp::F64Div => "f64.div",
        BinOp::F64Eq => "f64.eq",
        BinOp::F64Ne => "f64.ne",
        BinOp::F64Lt => "f64.lt",
        BinOp::F64Gt => "f64.gt",
        BinOp::F64Le => "f64.le",
        BinOp::F64Ge => "f64.ge",
        BinOp::I32And => "i32.and",
        BinOp::I32Or => "i32.or",
        BinOp::I64Shl => "i64.shl",
        BinOp::I64ShrU => "i64.shr_u",
        BinOp::I64Or => "i64.or",
    }
}

/// FNV-1a over the source bytes, hex. a content address, not a security
/// hash: it exists so the same program is one row rather than many, and a
/// dependency-free 64-bit hash is ample for a 200-entry log.
pub fn fingerprint(source: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in source.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// where the corpus lives, beside the cartridge state it describes.
pub fn corpus_path() -> String {
    "vanish-cartridges/corpus.json".to_string()
}
