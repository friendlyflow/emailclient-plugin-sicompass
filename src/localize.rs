//! The plugin's strings, in the user's language.
//!
//! They come from the app (`host::translate`), which has loaded this plugin's
//! `locales/<lang>.ftl` into its bundles. In the unit tests, which run outside
//! sicompass, the English bundle is read directly, so the tests see the same
//! text a user of the English app does. Every id starts with `emailclient-`,
//! which the app requires of a plugin's own strings.

/// Named arguments for a message, `{ $name }` in the Fluent source.
#[derive(Debug, Default, Clone)]
pub struct Args(Vec<(String, String)>);

impl Args {
    pub fn new() -> Self {
        Args::default()
    }

    pub fn set(&mut self, name: &str, value: impl ToString) {
        self.0.push((name.to_owned(), value.to_string()));
    }
}

#[cfg(not(test))]
pub fn t(key: &str) -> String {
    sicompass_sdk::plugin::host::translate(key)
}

#[cfg(not(test))]
pub fn t_args(key: &str, args: &Args) -> String {
    sicompass_sdk::plugin::host::translate_args(key, &args.0)
}

#[cfg(test)]
pub fn t(key: &str) -> String {
    t_args(key, &Args::default())
}

/// The English bundle, read the simple way: `id = text` lines, `{ $name }`
/// placeables. Enough for this plugin's strings; a missing id is the id.
#[cfg(test)]
pub fn t_args(key: &str, args: &Args) -> String {
    let source = include_str!("../locales/en-US.ftl");
    let Some(text) = source.lines().find_map(|l| {
        let (id, text) = l.split_once(" = ")?;
        (id.trim() == key).then_some(text)
    }) else {
        return key.to_owned();
    };
    let mut out = text.to_owned();
    for (name, value) in &args.0 {
        out = out.replace(&format!("{{ ${name} }}"), value);
    }
    out
}
