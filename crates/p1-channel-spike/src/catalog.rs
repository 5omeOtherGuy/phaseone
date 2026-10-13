//! The host's catalog: every event a plugin may subscribe to, with its schema version, and
//! every capability a plugin may ask for, with its methods. Adding an event or a method is a
//! row here and code on the host; the WIT world does not change.

/// Event name and the payload schema version the host emits.
pub const EVENTS: &[(&str, u32)] = &[
    ("tool/execute", 1),
    ("context/prepare", 1),
    ("session/start", 1),
    ("tool/post-execute", 1),
    ("tool/describe", 1),
];

/// Capability name and its methods.
pub const CAPABILITIES: &[(&str, &[&str])] = &[
    ("process", &["spawn", "next", "close"]),
    ("clock", &["now"]),
];

pub fn event_version(name: &str) -> Option<u32> {
    EVENTS
        .iter()
        .find(|(event, _)| *event == name)
        .map(|(_, version)| *version)
}

pub fn methods(capability: &str) -> Option<&'static [&'static str]> {
    CAPABILITIES
        .iter()
        .find(|(name, _)| *name == capability)
        .map(|(_, methods)| *methods)
}
