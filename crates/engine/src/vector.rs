//! pgvector emulation: the `vector`, `halfvec` and `sparsevec` types, their distance operators
//! and functions. Values are stored as canonical text (`[1,2,3]`, `{1:1.5,3:2}/5`); `halfvec`
//! elements are rounded to half precision on input, as pgvector stores them.

use rusqlite::Connection;
use rusqlite::functions::{Aggregate, Context, FunctionFlags};
use rusqlite::types::ValueRef;

use crate::error::EngineError;

/// pgvector's limits (`VECTOR_MAX_DIM`, `HALFVEC_MAX_DIM`, `SPARSEVEC_MAX_DIM`).
pub const MAX_DIM: usize = 16_000;
pub const MAX_SPARSE_DIM: usize = 1_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VectorKind {
    Vector,
    Halfvec,
    Sparsevec,
}

impl VectorKind {
    pub fn name(self) -> &'static str {
        match self {
            VectorKind::Vector => "vector",
            VectorKind::Halfvec => "halfvec",
            VectorKind::Sparsevec => "sparsevec",
        }
    }

    pub fn from_name(name: &str) -> Option<VectorKind> {
        match name.to_ascii_lowercase().as_str() {
            "vector" => Some(VectorKind::Vector),
            "halfvec" => Some(VectorKind::Halfvec),
            "sparsevec" => Some(VectorKind::Sparsevec),
            _ => None,
        }
    }

    /// Fixed OIDs for the extension types (real servers assign them at CREATE EXTENSION; clients
    /// look them up in `pg_type` by name).
    pub fn oid(self) -> u32 {
        match self {
            VectorKind::Vector => 16_390,
            VectorKind::Halfvec => 16_392,
            VectorKind::Sparsevec => 16_394,
        }
    }

    pub fn array_oid(self) -> u32 {
        self.oid() + 1
    }
}

/// A dense or sparse vector. Sparse vectors keep 0-based `indices` in ascending order.
#[derive(Debug, Clone, PartialEq)]
pub enum Vector {
    Dense(Vec<f32>),
    Sparse { dim: usize, indices: Vec<u32>, values: Vec<f32> },
}

fn data_err(msg: impl Into<String>) -> EngineError {
    EngineError::new("22P02", msg)
}

fn dim_err(msg: impl Into<String>) -> EngineError {
    EngineError::new("22000", msg)
}

impl Vector {
    pub fn dim(&self) -> usize {
        match self {
            Vector::Dense(v) => v.len(),
            Vector::Sparse { dim, .. } => *dim,
        }
    }

    /// Dense elements, refusing sparse vectors too large to expand (a client could otherwise make
    /// the server allocate gigabytes with `{1:1}/1000000000`).
    pub fn dense(&self) -> Result<Vec<f32>, EngineError> {
        if self.dim() > MAX_DIM {
            return Err(dim_err(format!("sparsevec with more than {MAX_DIM} dimensions cannot be used here")));
        }
        Ok(self.to_dense())
    }

    fn to_dense(&self) -> Vec<f32> {
        match self {
            Vector::Dense(v) => v.clone(),
            Vector::Sparse { dim, indices, values } => {
                let mut out = vec![0.0; *dim];
                for (i, v) in indices.iter().zip(values) {
                    out[*i as usize] = *v;
                }
                out
            }
        }
    }

    pub fn to_sparse(&self) -> Vector {
        match self {
            Vector::Sparse { .. } => self.clone(),
            Vector::Dense(v) => {
                let (mut indices, mut values) = (vec![], vec![]);
                for (i, x) in v.iter().enumerate() {
                    if *x != 0.0 {
                        indices.push(i as u32);
                        values.push(*x);
                    }
                }
                Vector::Sparse { dim: v.len(), indices, values }
            }
        }
    }

    /// Parse pgvector text input: `[1,2,3]` (also `{1,2,3}` arrays and JSON-ish spacing) or
    /// sparse `{1:1.5,3:2}/5` (1-based indices).
    pub fn parse(text: &str, kind: VectorKind) -> Result<Vector, EngineError> {
        let t = text.trim();
        let type_name = kind.name();
        if (kind == VectorKind::Sparsevec && !t.starts_with('[')) || (t.starts_with('{') && t.contains(':')) {
            return Self::parse_sparse(t, kind);
        }
        let inner = t
            .strip_prefix('[')
            .and_then(|r| r.strip_suffix(']'))
            .or_else(|| t.strip_prefix('{').and_then(|r| r.strip_suffix('}')))
            .ok_or_else(|| data_err(format!("invalid input syntax for type {type_name}: \"{t}\"")))?;
        let mut out = Vec::new();
        if !inner.trim().is_empty() {
            for part in inner.split(',') {
                let x: f32 = part.trim().parse().map_err(|_| data_err(format!("invalid input syntax for type {type_name}: \"{t}\"")))?;
                if !x.is_finite() {
                    return Err(dim_err(format!("{} not allowed in {type_name}", if x.is_nan() { "NaN" } else { "infinite value" })));
                }
                out.push(if kind == VectorKind::Halfvec { round_half(x)? } else { x });
            }
        }
        if out.is_empty() {
            return Err(dim_err(format!("{type_name} must have at least 1 dimension")));
        }
        if out.len() > MAX_DIM {
            return Err(dim_err(format!("{type_name} cannot have more than {MAX_DIM} dimensions")));
        }
        let dense = Vector::Dense(out);
        Ok(if kind == VectorKind::Sparsevec { dense.to_sparse() } else { dense })
    }

    fn parse_sparse(t: &str, kind: VectorKind) -> Result<Vector, EngineError> {
        let bad = || data_err(format!("invalid input syntax for type sparsevec: \"{t}\""));
        let (body, dim) = t.rsplit_once('/').ok_or_else(bad)?;
        let dim: usize = dim.trim().parse().map_err(|_| bad())?;
        if dim == 0 {
            return Err(dim_err("sparsevec must have at least 1 dimension"));
        }
        if dim > MAX_SPARSE_DIM {
            return Err(dim_err(format!("sparsevec cannot have more than {MAX_SPARSE_DIM} dimensions")));
        }
        let inner = body.trim().strip_prefix('{').and_then(|r| r.strip_suffix('}')).ok_or_else(bad)?;
        let mut pairs: Vec<(u32, f32)> = Vec::new();
        for part in inner.split(',').filter(|p| !p.trim().is_empty()) {
            let (i, v) = part.split_once(':').ok_or_else(bad)?;
            let i: u64 = i.trim().parse().map_err(|_| bad())?;
            let v: f32 = v.trim().parse().map_err(|_| bad())?;
            if i == 0 || i as usize > dim {
                return Err(dim_err("sparsevec index out of bounds"));
            }
            if !v.is_finite() {
                return Err(dim_err("NaN or infinite value not allowed in sparsevec"));
            }
            if v != 0.0 {
                pairs.push(((i - 1) as u32, v));
            }
        }
        pairs.sort_by_key(|p| p.0);
        if pairs.windows(2).any(|w| w[0].0 == w[1].0) {
            return Err(dim_err("sparsevec indices must not contain duplicates"));
        }
        let v = Vector::Sparse { dim, indices: pairs.iter().map(|p| p.0).collect(), values: pairs.iter().map(|p| p.1).collect() };
        Ok(if kind == VectorKind::Sparsevec { v } else { Vector::Dense(v.dense()?) })
    }

    /// Convert to `kind` (dense ⇄ sparse, rounding to half precision for `halfvec`).
    pub fn convert(&self, kind: VectorKind) -> Result<Vector, EngineError> {
        Ok(match kind {
            VectorKind::Sparsevec => self.to_sparse(),
            VectorKind::Vector => Vector::Dense(self.dense()?),
            VectorKind::Halfvec => Vector::Dense(self.dense()?.into_iter().map(round_half).collect::<Result<_, _>>()?),
        })
    }

    /// (index, value) pairs of the non-zero elements, in index order.
    fn nonzero(&self) -> Vec<(u32, f32)> {
        match self {
            Vector::Dense(v) => v.iter().enumerate().filter(|(_, x)| **x != 0.0).map(|(i, x)| (i as u32, *x)).collect(),
            Vector::Sparse { indices, values, .. } => indices.iter().copied().zip(values.iter().copied()).collect(),
        }
    }

    /// Canonical text, as pgvector prints it.
    pub fn to_text(&self) -> String {
        match self {
            Vector::Dense(v) => format!("[{}]", v.iter().map(|x| fmt_f32(*x)).collect::<Vec<_>>().join(",")),
            Vector::Sparse { dim, indices, values } => format!(
                "{{{}}}/{dim}",
                indices.iter().zip(values).map(|(i, v)| format!("{}:{}", i + 1, fmt_f32(*v))).collect::<Vec<_>>().join(",")
            ),
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Vector::Dense(v) => serde_json::Value::Array(v.iter().map(|x| serde_json::json!(*x as f64)).collect()),
            Vector::Sparse { .. } => serde_json::Value::String(self.to_text()),
        }
    }

    /// pgvector binary send format.
    pub fn to_binary(&self, kind: VectorKind) -> Vec<u8> {
        let mut out = Vec::new();
        match (kind, self) {
            (VectorKind::Sparsevec, _) => {
                let Vector::Sparse { dim, indices, values } = self.to_sparse() else { unreachable!() };
                out.extend((dim as i32).to_be_bytes());
                out.extend((indices.len() as i32).to_be_bytes());
                out.extend(0i32.to_be_bytes());
                for i in &indices {
                    out.extend((*i as i32).to_be_bytes());
                }
                for v in &values {
                    out.extend(v.to_be_bytes());
                }
            }
            (VectorKind::Halfvec, _) => {
                let d = self.dense().unwrap_or_default();
                out.extend((d.len() as i16).to_be_bytes());
                out.extend(0i16.to_be_bytes());
                for x in d {
                    out.extend(f32_to_f16(x).to_be_bytes());
                }
            }
            (VectorKind::Vector, _) => {
                let d = self.dense().unwrap_or_default();
                out.extend((d.len() as i16).to_be_bytes());
                out.extend(0i16.to_be_bytes());
                for x in d {
                    out.extend(x.to_be_bytes());
                }
            }
        }
        out
    }

    /// pgvector binary receive format.
    pub fn from_binary(b: &[u8], kind: VectorKind) -> Result<Vector, EngineError> {
        let bad = || data_err(format!("invalid binary data for type {}", kind.name()));
        let i16_at = |p: usize| b.get(p..p + 2).map(|s| i16::from_be_bytes([s[0], s[1]]));
        let i32_at = |p: usize| b.get(p..p + 4).map(|s| i32::from_be_bytes([s[0], s[1], s[2], s[3]]));
        match kind {
            VectorKind::Sparsevec => {
                let dim = i32_at(0).ok_or_else(bad)?;
                let nnz = i32_at(4).ok_or_else(bad)?;
                if dim <= 0 || nnz < 0 || b.len() != 12 + nnz as usize * 8 {
                    return Err(bad());
                }
                let n = nnz as usize;
                let indices: Vec<u32> = (0..n).map(|k| i32_at(12 + k * 4).unwrap() as u32).collect();
                let values: Vec<f32> = (0..n).map(|k| f32::from_bits(i32_at(12 + n * 4 + k * 4).unwrap() as u32)).collect();
                Ok(Vector::Sparse { dim: dim as usize, indices, values })
            }
            VectorKind::Vector | VectorKind::Halfvec => {
                let dim = i16_at(0).ok_or_else(bad)?;
                let width = if kind == VectorKind::Halfvec { 2 } else { 4 };
                if dim <= 0 || b.len() != 4 + dim as usize * width {
                    return Err(bad());
                }
                Ok(Vector::Dense(
                    (0..dim as usize)
                        .map(|k| {
                            let p = 4 + k * width;
                            if width == 2 { f16_to_f32(i16_at(p).unwrap() as u16) } else { f32::from_bits(i32_at(p).unwrap() as u32) }
                        })
                        .collect(),
                ))
            }
        }
    }
}

/// Shortest text that round-trips the f32 (what pgvector prints).
fn fmt_f32(x: f32) -> String {
    let s = format!("{x}");
    if s.contains('e') { format!("{x:e}") } else { s }
}

/// IEEE 754 binary16 encoding (round to nearest even).
pub fn f32_to_f16(x: f32) -> u16 {
    let bits = x.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32;
    let mant = bits & 0x7f_ffff;
    if exp == 0xff {
        return sign | 0x7c00 | if mant != 0 { 0x200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    if e <= 0 {
        if e < -10 {
            return sign;
        }
        let m = mant | 0x80_0000;
        let shift = (14 - e) as u32;
        let half = 1u32 << (shift - 1);
        let rem = m & ((1 << shift) - 1);
        let mut r = m >> shift;
        if rem > half || (rem == half && r & 1 == 1) {
            r += 1;
        }
        return sign | r as u16;
    }
    let mut r = ((e as u32) << 10) | (mant >> 13);
    let rem = mant & 0x1fff;
    if rem > 0x1000 || (rem == 0x1000 && r & 1 == 1) {
        r += 1;
    }
    sign | r as u16
}

pub fn f16_to_f32(h: u16) -> f32 {
    let sign = ((h & 0x8000) as u32) << 16;
    let exp = ((h >> 10) & 0x1f) as u32;
    let mant = (h & 0x3ff) as u32;
    let bits = if exp == 0 {
        if mant == 0 {
            sign
        } else {
            // Subnormal: normalize.
            let mut e = 127 - 15 + 1;
            let mut m = mant;
            while m & 0x400 == 0 {
                m <<= 1;
                e -= 1;
            }
            sign | ((e as u32) << 23) | ((m & 0x3ff) << 13)
        }
    } else if exp == 0x1f {
        sign | 0x7f80_0000 | (mant << 13)
    } else {
        sign | ((exp + 127 - 15) << 23) | (mant << 13)
    };
    f32::from_bits(bits)
}

/// Round to the nearest half-precision value; out of range is an error, like pgvector.
fn round_half(x: f32) -> Result<f32, EngineError> {
    let h = f16_to_f32(f32_to_f16(x));
    if h.is_infinite() && x.is_finite() {
        return Err(dim_err(format!("\"{x}\" is out of range for type halfvec")));
    }
    Ok(h)
}

/// Read a vector argument (any kind) from SQLite text. NULL → None.
fn arg(ctx: &Context<'_>, i: usize) -> rusqlite::Result<Option<Vector>> {
    let text = match ctx.get_raw(i) {
        ValueRef::Null => return Ok(None),
        ValueRef::Text(t) | ValueRef::Blob(t) => String::from_utf8_lossy(t).into_owned(),
        ValueRef::Integer(n) => n.to_string(),
        ValueRef::Real(r) => r.to_string(),
    };
    let kind = if text.trim_start().starts_with('{') && text.contains('/') { VectorKind::Sparsevec } else { VectorKind::Vector };
    Vector::parse(&text, kind).map(Some).map_err(user_err)
}

fn user_err(e: EngineError) -> rusqlite::Error {
    rusqlite::Error::UserFunctionError(Box::new(e))
}

fn same_dims(a: &Vector, b: &Vector) -> rusqlite::Result<()> {
    if a.dim() != b.dim() {
        return Err(user_err(dim_err(format!("different vector dimensions {} and {}", a.dim(), b.dim()))));
    }
    Ok(())
}

/// Distance kinds behind the pgvector operators and functions.
#[derive(Clone, Copy)]
enum Metric {
    L2,
    InnerProduct,
    NegativeInnerProduct,
    Cosine,
    L1,
}

fn distance(a: &Vector, b: &Vector, m: Metric) -> f64 {
    // Walk the union of non-zero positions: zeros on both sides add nothing to any metric.
    let (x, y) = (a.nonzero(), b.nonzero());
    let mut merged: Vec<(f64, f64)> = Vec::with_capacity(x.len() + y.len());
    let (mut i, mut j) = (0, 0);
    while i < x.len() || j < y.len() {
        match (x.get(i), y.get(j)) {
            (Some(p), Some(q)) if p.0 == q.0 => {
                merged.push((p.1 as f64, q.1 as f64));
                i += 1;
                j += 1;
            }
            (Some(p), Some(q)) if p.0 < q.0 => {
                merged.push((p.1 as f64, 0.0));
                i += 1;
            }
            (Some(_), Some(q)) => {
                merged.push((0.0, q.1 as f64));
                j += 1;
            }
            (Some(p), None) => {
                merged.push((p.1 as f64, 0.0));
                i += 1;
            }
            (None, Some(q)) => {
                merged.push((0.0, q.1 as f64));
                j += 1;
            }
            (None, None) => break,
        }
    }
    let pairs = merged.into_iter();
    match m {
        Metric::L2 => pairs.map(|(p, q)| (p - q) * (p - q)).sum::<f64>().sqrt(),
        Metric::L1 => pairs.map(|(p, q)| (p - q).abs()).sum(),
        Metric::InnerProduct => pairs.map(|(p, q)| p * q).sum(),
        Metric::NegativeInnerProduct => -pairs.map(|(p, q)| p * q).sum::<f64>(),
        Metric::Cosine => {
            let (mut dot, mut na, mut nb) = (0.0, 0.0, 0.0);
            for (p, q) in pairs {
                dot += p * q;
                na += p * p;
                nb += q * q;
            }
            if na == 0.0 || nb == 0.0 { f64::NAN } else { 1.0 - (dot / (na.sqrt() * nb.sqrt())).clamp(-1.0, 1.0) }
        }
    }
}

fn binary_fn(conn: &Connection, name: &str, m: Metric) -> rusqlite::Result<()> {
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    conn.create_scalar_function(name, 2, flags, move |ctx| {
        let (Some(a), Some(b)) = (arg(ctx, 0)?, arg(ctx, 1)?) else { return Ok(None) };
        same_dims(&a, &b)?;
        let d = distance(&a, &b, m);
        Ok(if d.is_nan() { None } else { Some(d) })
    })
}

/// Element-wise sum and count for `avg(vector)` / `sum(vector)`.
struct VecAgg {
    average: bool,
}

impl Aggregate<(Vec<f64>, usize), Option<String>> for VecAgg {
    fn init(&self, _: &mut Context<'_>) -> rusqlite::Result<(Vec<f64>, usize)> {
        Ok((Vec::new(), 0))
    }

    fn step(&self, ctx: &mut Context<'_>, acc: &mut (Vec<f64>, usize)) -> rusqlite::Result<()> {
        let Some(v) = arg(ctx, 0)? else { return Ok(()) };
        let d = v.dense().map_err(user_err)?;
        if acc.1 == 0 {
            acc.0 = vec![0.0; d.len()];
        } else if acc.0.len() != d.len() {
            return Err(user_err(dim_err(format!("expected {} dimensions, not {}", acc.0.len(), d.len()))));
        }
        for (s, x) in acc.0.iter_mut().zip(d) {
            *s += x as f64;
        }
        acc.1 += 1;
        Ok(())
    }

    fn finalize(&self, _: &mut Context<'_>, acc: Option<(Vec<f64>, usize)>) -> rusqlite::Result<Option<String>> {
        let Some((sum, n)) = acc.filter(|a| a.1 > 0) else { return Ok(None) };
        let div = if self.average { n as f64 } else { 1.0 };
        Ok(Some(Vector::Dense(sum.into_iter().map(|s| (s / div) as f32).collect()).to_text()))
    }
}

/// Register the vector functions used by the translator (`tb_vec_*`) on a session connection.
pub fn register(conn: &Connection) -> rusqlite::Result<()> {
    let det = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    binary_fn(conn, "tb_vec_l2", Metric::L2)?;
    binary_fn(conn, "tb_vec_ip", Metric::InnerProduct)?;
    binary_fn(conn, "tb_vec_neg_ip", Metric::NegativeInnerProduct)?;
    binary_fn(conn, "tb_vec_cos", Metric::Cosine)?;
    binary_fn(conn, "tb_vec_l1", Metric::L1)?;
    conn.create_scalar_function("tb_vec_dims", 1, det, |ctx| Ok(arg(ctx, 0)?.map(|v| v.dim() as i64)))?;
    conn.create_scalar_function("tb_vec_norm", 1, det, |ctx| {
        Ok(arg(ctx, 0)?.map(|v| v.nonzero().iter().map(|(_, x)| (*x as f64) * (*x as f64)).sum::<f64>().sqrt()))
    })?;
    // tb_vec_normalize(v, kind)
    conn.create_scalar_function("tb_vec_normalize", 2, det, |ctx| {
        let Some(v) = arg(ctx, 0)? else { return Ok(None) };
        let kind = VectorKind::from_name(&ctx.get::<String>(1)?).unwrap_or(VectorKind::Vector);
        let d = v.dense().map_err(user_err)?;
        let n = d.iter().map(|x| (*x as f64) * (*x as f64)).sum::<f64>().sqrt();
        let out = Vector::Dense(if n == 0.0 { d } else { d.iter().map(|x| (*x as f64 / n) as f32).collect() });
        Ok(Some(out.convert(kind).map_err(user_err)?.to_text()))
    })?;
    // tb_vec_sub(v, start (1-based), count, kind)
    conn.create_scalar_function("tb_vec_subvector", 4, det, |ctx| {
        let Some(v) = arg(ctx, 0)? else { return Ok(None) };
        let start: i64 = ctx.get(1)?;
        let count: i64 = ctx.get(2)?;
        let kind = VectorKind::from_name(&ctx.get::<String>(3)?).unwrap_or(VectorKind::Vector);
        let d = v.dense().map_err(user_err)?;
        let from = (start.max(1) - 1) as usize;
        let to = ((start + count - 1).min(d.len() as i64)).max(0) as usize;
        if count < 1 || from >= to {
            return Err(user_err(dim_err(format!("{} must have at least 1 dimension", kind.name()))));
        }
        Ok(Some(Vector::Dense(d[from..to].to_vec()).convert(kind).map_err(user_err)?.to_text()))
    })?;
    // Element-wise arithmetic: tb_vec_arith(a, b, op, kind)
    conn.create_scalar_function("tb_vec_arith", 4, det, |ctx| {
        let (Some(a), Some(b)) = (arg(ctx, 0)?, arg(ctx, 1)?) else { return Ok(None) };
        let op: String = ctx.get(2)?;
        let kind = VectorKind::from_name(&ctx.get::<String>(3)?).unwrap_or(VectorKind::Vector);
        let (x, y) = (a.dense().map_err(user_err)?, b.dense().map_err(user_err)?);
        let out: Vec<f32> = if op == "||" {
            x.into_iter().chain(y).collect()
        } else {
            same_dims(&a, &b)?;
            x.iter()
                .zip(&y)
                .map(|(p, q)| match op.as_str() {
                    "+" => p + q,
                    "-" => p - q,
                    _ => p * q,
                })
                .collect()
        };
        if out.iter().any(|v| !v.is_finite()) {
            return Err(user_err(dim_err("value out of range: overflow")));
        }
        Ok(Some(Vector::Dense(out).convert(kind).map_err(user_err)?.to_text()))
    })?;
    // binary_quantize(vector) → bit string; hamming / jaccard on bit strings.
    conn.create_scalar_function("tb_vec_binary_quantize", 1, det, |ctx| {
        let Some(v) = arg(ctx, 0)? else { return Ok(None) };
        Ok(Some(v.dense().map_err(user_err)?.iter().map(|x| if *x > 0.0 { '1' } else { '0' }).collect::<String>()))
    })?;
    conn.create_scalar_function("tb_bit_hamming", 2, det, |ctx| {
        let (a, b): (Option<String>, Option<String>) = (ctx.get(0)?, ctx.get(1)?);
        let (Some(a), Some(b)) = (a, b) else { return Ok(None) };
        if a.len() != b.len() {
            return Err(user_err(dim_err(format!("different bit lengths {} and {}", a.len(), b.len()))));
        }
        Ok(Some(a.chars().zip(b.chars()).filter(|(x, y)| x != y).count() as f64))
    })?;
    conn.create_scalar_function("tb_bit_jaccard", 2, det, |ctx| {
        let (a, b): (Option<String>, Option<String>) = (ctx.get(0)?, ctx.get(1)?);
        let (Some(a), Some(b)) = (a, b) else { return Ok(None) };
        if a.len() != b.len() {
            return Err(user_err(dim_err(format!("different bit lengths {} and {}", a.len(), b.len()))));
        }
        let (mut both, mut either) = (0usize, 0usize);
        for (x, y) in a.chars().zip(b.chars()) {
            both += (x == '1' && y == '1') as usize;
            either += (x == '1' || y == '1') as usize;
        }
        Ok(Some(if either == 0 { 0.0 } else { 1.0 - both as f64 / either as f64 }))
    })?;
    conn.create_aggregate_function("tb_vec_avg", 1, det, VecAgg { average: true })?;
    conn.create_aggregate_function("tb_vec_sum", 1, det, VecAgg { average: false })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_prints_like_pgvector() {
        assert_eq!(Vector::parse(" [1, 2.5,-3] ", VectorKind::Vector).unwrap().to_text(), "[1,2.5,-3]");
        assert_eq!(Vector::parse("[0.1]", VectorKind::Halfvec).unwrap().to_text(), "[0.099975586]");
        assert_eq!(Vector::parse("{1:1.5,3:2}/5", VectorKind::Sparsevec).unwrap().to_text(), "{1:1.5,3:2}/5");
        assert_eq!(
            Vector::parse("[0,1.5,0,2,0]", VectorKind::Vector).unwrap().convert(VectorKind::Sparsevec).unwrap().to_text(),
            "{2:1.5,4:2}/5"
        );
        assert!(Vector::parse("[]", VectorKind::Vector).is_err());
        assert!(Vector::parse("[1,NaN]", VectorKind::Vector).is_err());
        assert!(Vector::parse("[70000]", VectorKind::Halfvec).is_err());
        assert!(Vector::parse("{6:1}/5", VectorKind::Sparsevec).is_err());
    }

    #[test]
    fn half_precision_round_trips() {
        for x in [0.0f32, 1.0, -2.5, 65504.0, 0.000061035156, 1e-7, 0.333] {
            let h = f16_to_f32(f32_to_f16(x));
            assert!((h - x).abs() <= x.abs() * 0.001 + 1e-7, "{x} -> {h}");
        }
    }

    #[test]
    fn binary_formats_round_trip() {
        for (text, kind) in [("[1,2,3]", VectorKind::Vector), ("[1,0.5]", VectorKind::Halfvec), ("{2:4}/3", VectorKind::Sparsevec)] {
            let v = Vector::parse(text, kind).unwrap();
            assert_eq!(Vector::from_binary(&v.to_binary(kind), kind).unwrap().to_text(), v.to_text());
        }
    }

    #[test]
    fn huge_sparse_vectors_are_not_expanded() {
        let a = Vector::parse("{1:1,999999999:2}/1000000000", VectorKind::Sparsevec).unwrap();
        let b = Vector::parse("{1:2}/1000000000", VectorKind::Sparsevec).unwrap();
        assert!((distance(&a, &b, Metric::L2) - 5f64.sqrt()).abs() < 1e-9);
        assert!(a.convert(VectorKind::Vector).is_err());
        assert!(a.dense().is_err());
    }

    #[test]
    fn distances() {
        let a = Vector::parse("[1,0]", VectorKind::Vector).unwrap();
        let b = Vector::parse("[0,1]", VectorKind::Vector).unwrap();
        assert!((distance(&a, &b, Metric::L2) - 2f64.sqrt()).abs() < 1e-9);
        assert_eq!(distance(&a, &b, Metric::Cosine), 1.0);
        assert_eq!(distance(&a, &a, Metric::NegativeInnerProduct), -1.0);
        assert_eq!(distance(&a, &b, Metric::L1), 2.0);
    }
}

/// pgvector operators the SQL parser has no tokens for, swapped for geometric operator tokens the
/// translator maps back (`<#>` → `<^`, `<+>` → `>^`, `<~>` → `?-|`, `<%>` → `?||`). Strings,
/// quoted identifiers, comments and dollar-quoted bodies are left alone.
pub fn rewrite_operators(sql: &str) -> std::borrow::Cow<'_, str> {
    const OPS: [(&str, &str); 4] = [("<#>", "<^"), ("<+>", ">^"), ("<~>", "?-|"), ("<%>", "?||")];
    if !OPS.iter().any(|(from, _)| sql.contains(from)) {
        return std::borrow::Cow::Borrowed(sql);
    }
    let b = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    let copy_until = |out: &mut String, from: usize, to: usize| out.push_str(&sql[from..to]);
    while i < b.len() {
        let c = b[i];
        // Quoted text and identifiers.
        if c == b'\'' || c == b'"' {
            let start = i;
            i += 1;
            while i < b.len() {
                if b[i] == c {
                    if i + 1 < b.len() && b[i + 1] == c {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            copy_until(&mut out, start, i);
            continue;
        }
        if c == b'-' && b.get(i + 1) == Some(&b'-') {
            let start = i;
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            copy_until(&mut out, start, i);
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let start = i;
            let mut depth = 0;
            while i < b.len() {
                if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            copy_until(&mut out, start, i.min(b.len()));
            continue;
        }
        if c == b'$' {
            // $tag$ … $tag$
            let tag_end = sql[i + 1..].find('$').map(|p| i + 1 + p);
            if let Some(te) = tag_end
                && sql[i + 1..te].chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
                && !sql[i + 1..te].starts_with(|ch: char| ch.is_ascii_digit())
            {
                let tag = &sql[i..=te];
                let body_start = te + 1;
                let end = sql[body_start..].find(tag).map(|p| body_start + p + tag.len()).unwrap_or(sql.len());
                copy_until(&mut out, i, end);
                i = end;
                continue;
            }
        }
        if let Some((from, to)) = OPS.iter().find(|(from, _)| sql[i..].starts_with(from)) {
            out.push(' ');
            out.push_str(to);
            out.push(' ');
            i += from.len();
            continue;
        }
        let ch_len = sql[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        copy_until(&mut out, i, i + ch_len);
        i += ch_len;
    }
    std::borrow::Cow::Owned(out)
}

#[cfg(test)]
mod operator_tests {
    use super::rewrite_operators;

    #[test]
    fn rewrites_only_outside_strings_and_comments() {
        assert_eq!(rewrite_operators("SELECT 1"), "SELECT 1");
        assert_eq!(
            rewrite_operators("SELECT a <#> '[1]', '<#>' -- <+>\n, b<+>c /* <~> */ $$<%>$$"),
            "SELECT a  <^  '[1]', '<#>' -- <+>\n, b >^ c /* <~> */ $$<%>$$"
        );
    }
}
