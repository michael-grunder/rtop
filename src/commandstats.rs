/// Columns available in the commandstats detail table, in their default order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandstatsColumn {
    Command,
    Calls,
    Usec,
    UsecPerCall,
}

impl CommandstatsColumn {
    pub const ALL: [Self; 4] = [Self::Command, Self::Calls, Self::Usec, Self::UsecPerCall];

    pub const fn header(self) -> &'static str {
        match self {
            Self::Command => "Command",
            Self::Calls => "Calls",
            Self::Usec => "Usec",
            Self::UsecPerCall => "Usec/Call",
        }
    }
}
