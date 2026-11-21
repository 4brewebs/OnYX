use std::fmt;

/// Error returned by `onyx-scent` indexing, querying, or TOON encoding operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScentError {
    Io { path: String, message: String },
    Parse { path: String, message: String },
    TargetNotFound { target: String },
    InvalidToon { line: usize, message: String },
}

impl fmt::Display for ScentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, message } => write!(f, "I/O error at `{path}`: {message}"),
            Self::Parse { path, message } => write!(f, "AST parse error at `{path}`: {message}"),
            Self::TargetNotFound { target } => {
                write!(f, "symbol target `{target}` not found in workspace index")
            }
            Self::InvalidToon { line, message } => {
                write!(f, "invalid TOON record at line {line}: {message}")
            }
        }
    }
}

impl std::error::Error for ScentError {}

/// Source language supported by the AST extractor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SupportedLang {
    Rust,
    Python,
    Java,
}

impl SupportedLang {
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "rs" => Some(Self::Rust),
            "py" => Some(Self::Python),
            "java" => Some(Self::Java),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::Java => "java",
        }
    }
}

/// Kind of symbol extracted into the `ScentArena`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EntityKind {
    Function,
    Method,
    Struct,
    Enum,
    Trait,
    Class,
    Interface,
}

impl EntityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Function => "fn",
            Self::Method => "method",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::Class => "class",
            Self::Interface => "interface",
        }
    }

    pub fn is_callable(self) -> bool {
        matches!(self, Self::Function | Self::Method)
    }
}

/// Semantic dependency relation between two symbols.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RelationKind {
    Calls,
    CalledBy,
    Implements,
    UsesType,
    UsedByType,
}

impl RelationKind {
    /// Thermal conductivity in basis points (`0..=10_000`).
    pub fn default_weight_bp(self) -> u16 {
        match self {
            Self::Calls => 5_500,
            Self::CalledBy => 9_000,
            Self::Implements => 8_500,
            Self::UsesType => 6_500,
            Self::UsedByType => 8_000,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Calls => "calls",
            Self::CalledBy => "called_by",
            Self::Implements => "implements",
            Self::UsesType => "uses_type",
            Self::UsedByType => "used_by",
        }
    }
}

/// Computes a deterministic 64-bit FNV-1a hash of a symbol's semantic signature
/// `(kind, visibility, signature)`, intentionally excluding line numbers so moving
/// code without changing its contract never causes false-positive stale invalidation.
pub fn compute_signature_hash(kind: EntityKind, is_public: bool, signature: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    let mut feed = |bytes: &[u8]| {
        for &b in bytes {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(FNV_PRIME);
    };
    feed(kind.as_str().as_bytes());
    feed(if is_public { b"pub" } else { b"priv" });
    feed(signature.trim().as_bytes());
    hash
}

/// Single AST symbol stored in the contiguous workspace arena.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entity {
    pub id: usize,
    pub qualified_name: String,
    pub short_name: String,
    pub owner_type: Option<String>,
    pub file: String,
    pub lang: SupportedLang,
    pub kind: EntityKind,
    pub signature: String,
    /// Deterministic FNV-1a hash over `(kind, is_public, signature)` (line-number invariant).
    pub signature_hash: u64,
    pub is_public: bool,
    pub is_test: bool,
    pub start_line: usize,
    pub end_line: usize,
}

/// Directed weighted semantic edge between two arena entities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Edge {
    pub source_id: usize,
    pub target_id: usize,
    pub kind: RelationKind,
    /// Weight in basis points (`0..=10_000`).
    pub weight_bp: u16,
}

/// Contiguous deterministic AST graph of a workspace or path slice.
#[derive(Debug, Default, Clone)]
pub struct ScentArena {
    pub nodes: Vec<Entity>,
    pub edges: Vec<Edge>,
}

impl ScentArena {
    pub fn new() -> Self {
        Self::default()
    }
}
