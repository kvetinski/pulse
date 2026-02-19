use std::collections::HashMap;

#[derive(Default, Debug)]
pub struct ScenarioContext {
    variables: HashMap<String, String>,
}

impl ScenarioContext {
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.variables.insert(key.into(), value.into());
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.variables.get(key).map(String::as_str)
    }
}
