//! Which of the two vector columns a unit reads. A slot keeps its model from naming until retire.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VectorSlot {
    /// `memory.embedding`.
    #[default]
    A,
    /// `memory.embedding_b`.
    B,
}

impl VectorSlot {
    /// The column name. Adapters use it only in tests; statements carry their column as a literal.
    pub const fn column(self) -> &'static str {
        match self {
            Self::A => "embedding",
            Self::B => "embedding_b",
        }
    }

    pub const fn other(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }

    /// The value `embedding_state.active_slot` stores.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::A => "a",
            Self::B => "b",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "a" => Some(Self::A),
            "b" => Some(Self::B),
            _ => None,
        }
    }
}
