/// Default columns and additional metrics discovered in INFO COMMANDSTATS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandstatsColumn {
    Command,
    Calls,
    Usec,
    UsecPerCall,
    Metric(String),
}

impl CommandstatsColumn {
    pub const DEFAULT: [Self; 4] = [Self::Command, Self::Calls, Self::Usec, Self::UsecPerCall];

    pub fn header(&self) -> &str {
        match self {
            Self::Command => "Command",
            Self::Calls => "Calls",
            Self::Usec => "Usec",
            Self::UsecPerCall => "Usec/Call",
            Self::Metric(name) => name,
        }
    }
}
