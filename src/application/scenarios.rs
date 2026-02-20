use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use serde::Deserialize;
use serde_json::Value;

use crate::application::steps::{DynamicGrpcStep, GrpcRequestSpec};
use crate::domain::contracts::PartitionKeyStrategy;
use crate::domain::scenario::{RepeatPolicy, Scenario, ScenarioConfig, Step};
use crate::infrastructure::config::AppConfig;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScenarioFile {
    version: u16,
    scenarios: Vec<ScenarioEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScenarioEntry {
    name: String,
    endpoint: Option<String>,
    scenarios_per_sec: f64,
    max_concurrency: usize,
    duration: String,
    repeat: RepeatEntry,
    #[serde(default)]
    partition_key_strategy: Option<PartitionKeyStrategyEntry>,
    steps: Vec<StepEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum RepeatEntry {
    Once,
    Every { interval: String },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PartitionKeyStrategyEntry {
    ScenarioId,
    ExecutionKey,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "protocol", rename_all = "snake_case", deny_unknown_fields)]
enum StepEntry {
    Grpc {
        #[serde(default)]
        endpoint: Option<String>,
        service: String,
        method: String,
        #[serde(default)]
        request_base64: Option<String>,
        #[serde(default)]
        request_fields: Option<Value>,
        #[serde(default)]
        extract: Option<std::collections::HashMap<String, String>>,
        #[serde(default)]
        response_payload_context_key: Option<String>,
    },
    Http {
        method: String,
        url: String,
    },
}

pub fn load_scenarios(config: &AppConfig) -> Result<Vec<Scenario>, String> {
    match config.scenarios_file.as_deref() {
        Some(path) => load_scenarios_from_yaml(path, &config.pulse_endpoint),
        None => {
            let default_path = "scenarios.yaml";
            if Path::new(default_path).exists() {
                load_scenarios_from_yaml(default_path, &config.pulse_endpoint)
            } else {
                Err(
                    "no scenario source found; set PULSE_SCENARIOS_FILE or add ./scenarios.yaml"
                        .to_string(),
                )
            }
        }
    }
}

pub fn load_scenarios_from_yaml(
    path: &str,
    default_endpoint: &str,
) -> Result<Vec<Scenario>, String> {
    let raw = fs::read_to_string(path)
        .map_err(|e| format!("failed to read scenarios file '{path}': {e}"))?;
    parse_scenarios_yaml(&raw, default_endpoint)
        .map_err(|e| format!("failed to load scenarios from '{path}': {e}"))
}

fn parse_scenarios_yaml(raw: &str, default_endpoint: &str) -> Result<Vec<Scenario>, String> {
    let parsed: ScenarioFile =
        serde_yaml::from_str(raw).map_err(|e| format!("invalid yaml: {e}"))?;

    let validation_errors = validate_schema(&parsed);
    if !validation_errors.is_empty() {
        let mut message = String::from("scenario schema validation failed:");
        for err in validation_errors {
            message.push_str("\n - ");
            message.push_str(&err);
        }
        return Err(message);
    }

    parsed
        .scenarios
        .into_iter()
        .map(|entry| scenario_entry_into_domain(entry, default_endpoint))
        .collect()
}

fn validate_schema(file: &ScenarioFile) -> Vec<String> {
    let mut errors = Vec::new();
    if file.version != 1 {
        errors.push(format!(
            "unsupported scenarios file version '{}' (expected 1)",
            file.version
        ));
    }
    if file.scenarios.is_empty() {
        errors.push("scenarios must contain at least one item".to_string());
        return errors;
    }

    let mut scenario_names = HashSet::new();
    for (i, scenario) in file.scenarios.iter().enumerate() {
        let prefix = format!("scenarios[{i}]");

        if scenario.name.trim().is_empty() {
            errors.push(format!("{prefix}.name must not be empty"));
        } else if !scenario_names.insert(scenario.name.trim().to_string()) {
            errors.push(format!("{prefix}.name '{}' is duplicated", scenario.name));
        }

        if let Some(endpoint) = &scenario.endpoint
            && endpoint.trim().is_empty()
        {
            errors.push(format!("{prefix}.endpoint must not be empty when provided"));
        }
        if !scenario.scenarios_per_sec.is_finite() || scenario.scenarios_per_sec <= 0.0 {
            errors.push(format!("{prefix}.scenarios_per_sec must be > 0"));
        }
        if scenario.max_concurrency == 0 {
            errors.push(format!("{prefix}.max_concurrency must be > 0"));
        }
        if parse_duration_literal(&scenario.duration).is_err() {
            errors.push(format!(
                "{prefix}.duration '{}' must use Ns/Nm/Nh format",
                scenario.duration
            ));
        }

        match &scenario.repeat {
            RepeatEntry::Once => {}
            RepeatEntry::Every { interval } => {
                if parse_duration_literal(interval).is_err() {
                    errors.push(format!(
                        "{prefix}.repeat.interval '{}' must use Ns/Nm/Nh format",
                        interval
                    ));
                }
            }
        }

        if scenario.steps.is_empty() {
            errors.push(format!("{prefix}.steps must contain at least one step"));
        }

        for (j, step) in scenario.steps.iter().enumerate() {
            let step_prefix = format!("{prefix}.steps[{j}]");
            match step {
                StepEntry::Grpc {
                    endpoint,
                    service,
                    method,
                    request_base64,
                    request_fields,
                    extract,
                    response_payload_context_key,
                } => {
                    if let Some(step_endpoint) = endpoint
                        && step_endpoint.trim().is_empty()
                    {
                        errors.push(format!(
                            "{step_prefix}.endpoint must not be empty when provided"
                        ));
                    }
                    if service.trim().is_empty() {
                        errors.push(format!("{step_prefix}.service must not be empty"));
                    }
                    if method.trim().is_empty() {
                        errors.push(format!("{step_prefix}.method must not be empty"));
                    }
                    if let Some(raw) = request_base64 {
                        if raw.trim().is_empty() {
                            errors.push(format!(
                                "{step_prefix}.request_base64 must not be empty when provided"
                            ));
                        } else if !raw.contains("${")
                            && base64::engine::general_purpose::STANDARD
                                .decode(raw.as_bytes())
                                .is_err()
                        {
                            errors.push(format!(
                                "{step_prefix}.request_base64 must be valid base64 payload"
                            ));
                        }
                    }
                    if request_base64.is_some() && request_fields.is_some() {
                        errors.push(format!(
                            "{step_prefix} must define only one of request_base64 or request_fields"
                        ));
                    }
                    if let Some(fields) = request_fields
                        && !fields.is_object()
                    {
                        errors.push(format!(
                            "{step_prefix}.request_fields must be a JSON object"
                        ));
                    }
                    if let Some(extract_map) = extract {
                        for (ctx_key, response_path) in extract_map {
                            if ctx_key.trim().is_empty() {
                                errors.push(format!(
                                    "{step_prefix}.extract contains empty context key"
                                ));
                            }
                            if response_path.trim().is_empty() {
                                errors.push(format!(
                                    "{step_prefix}.extract path for '{ctx_key}' must not be empty"
                                ));
                            }
                        }
                    }
                    if let Some(key) = response_payload_context_key
                        && key.trim().is_empty()
                    {
                        errors.push(format!(
                            "{step_prefix}.response_payload_context_key must not be empty when provided"
                        ));
                    }
                }
                StepEntry::Http { method, url } => {
                    if method.trim().is_empty() {
                        errors.push(format!("{step_prefix}.method must not be empty"));
                    }
                    if url.trim().is_empty() {
                        errors.push(format!("{step_prefix}.url must not be empty"));
                    }
                }
            }
        }
    }
    errors
}

fn scenario_entry_into_domain(
    entry: ScenarioEntry,
    default_endpoint: &str,
) -> Result<Scenario, String> {
    let steps: Vec<Arc<dyn Step>> = entry
        .steps
        .into_iter()
        .map(step_entry_into_domain)
        .collect::<Result<Vec<_>, _>>()?;

    let partition_key_strategy = match entry.partition_key_strategy {
        Some(PartitionKeyStrategyEntry::ScenarioId) => PartitionKeyStrategy::ScenarioId,
        Some(PartitionKeyStrategyEntry::ExecutionKey) | None => PartitionKeyStrategy::ExecutionKey,
    };

    let repeat = match entry.repeat {
        RepeatEntry::Once => RepeatPolicy::Once,
        RepeatEntry::Every { interval } => RepeatPolicy::Every(parse_duration_literal(&interval)?),
    };

    let endpoint = match entry.endpoint {
        Some(endpoint) => endpoint,
        None => default_endpoint.to_string(),
    };

    Ok(Scenario::new(
        entry.name,
        steps,
        ScenarioConfig {
            endpoint,
            scenarios_per_sec: entry.scenarios_per_sec,
            max_concurrency: entry.max_concurrency,
            duration: parse_duration_literal(&entry.duration)?,
            repeat,
            partition_key_strategy,
        },
    ))
}

fn step_entry_into_domain(step: StepEntry) -> Result<Arc<dyn Step>, String> {
    match step {
        StepEntry::Grpc {
            endpoint,
            service,
            method,
            request_base64,
            request_fields,
            extract,
            response_payload_context_key,
        } => {
            let request_spec = if let Some(raw) = request_base64 {
                GrpcRequestSpec::StaticBase64Template(raw)
            } else if let Some(fields) = request_fields {
                GrpcRequestSpec::FieldTemplate(fields)
            } else {
                GrpcRequestSpec::Empty
            };
            let extraction = extract.unwrap_or_default();

            Ok(Arc::new(DynamicGrpcStep::new(
                endpoint,
                service,
                method,
                request_spec,
                extraction,
                response_payload_context_key,
            )) as Arc<dyn Step>)
        }
        StepEntry::Http { .. } => Err(
            "http step declared in yaml, but HTTP adapter is not implemented yet (step 5)"
                .to_string(),
        ),
    }
}

fn parse_duration_literal(value: &str) -> Result<Duration, String> {
    if let Some(num) = value.strip_suffix('s') {
        let secs = num
            .parse::<u64>()
            .map_err(|_| format!("invalid seconds duration: {value}"))?;
        if secs == 0 {
            return Err(format!("duration must be > 0: {value}"));
        }
        return Ok(Duration::from_secs(secs));
    }
    if let Some(num) = value.strip_suffix('m') {
        let mins = num
            .parse::<u64>()
            .map_err(|_| format!("invalid minutes duration: {value}"))?;
        if mins == 0 {
            return Err(format!("duration must be > 0: {value}"));
        }
        return Ok(Duration::from_secs(mins * 60));
    }
    if let Some(num) = value.strip_suffix('h') {
        let hours = num
            .parse::<u64>()
            .map_err(|_| format!("invalid hours duration: {value}"))?;
        if hours == 0 {
            return Err(format!("duration must be > 0: {value}"));
        }
        return Ok(Duration::from_secs(hours * 3600));
    }
    Err(format!(
        "unsupported duration format: {value} (use Ns/Nm/Nh)"
    ))
}

#[cfg(test)]
mod tests {
    use super::parse_scenarios_yaml;

    #[test]
    fn loads_valid_yaml() {
        let yaml = r#"
version: 1
scenarios:
  - name: Smoke
    scenarios_per_sec: 1
    max_concurrency: 1
    duration: 5s
    repeat:
      type: once
    steps:
      - protocol: grpc
        service: account.v1.AccountService
        method: CreateAccount
        request_fields:
          phone: "${gen.phone}"
        extract:
          user_id: "account.id"
"#;
        let scenarios = parse_scenarios_yaml(yaml, "http://127.0.0.1:8080").expect("valid yaml");
        assert_eq!(scenarios.len(), 1);
        assert_eq!(scenarios[0].name, "Smoke");
    }

    #[test]
    fn rejects_duplicate_scenario_names() {
        let yaml = r#"
version: 1
scenarios:
  - name: Duplicate
    scenarios_per_sec: 1
    max_concurrency: 1
    duration: 5s
    repeat:
      type: once
    steps:
      - protocol: grpc
        service: account.v1.AccountService
        method: GetAccount
  - name: Duplicate
    scenarios_per_sec: 1
    max_concurrency: 1
    duration: 5s
    repeat:
      type: once
    steps:
      - protocol: grpc
        service: account.v1.AccountService
        method: CreateAccount
"#;
        let err = match parse_scenarios_yaml(yaml, "http://127.0.0.1:8080") {
            Ok(_) => panic!("must fail validation"),
            Err(err) => err,
        };
        assert!(err.contains("is duplicated"));
    }

    #[test]
    fn rejects_invalid_grpc_request_base64() {
        let yaml = r#"
version: 1
scenarios:
  - name: GrpcInvalidBase64
    scenarios_per_sec: 1
    max_concurrency: 1
    duration: 5s
    repeat:
      type: once
    steps:
      - protocol: grpc
        service: account.v1.AccountService
        method: CreateAccount
        request_base64: "%%%notbase64%%%"
"#;
        let err = match parse_scenarios_yaml(yaml, "http://127.0.0.1:8080") {
            Ok(_) => panic!("must fail schema validation"),
            Err(err) => err,
        };
        assert!(err.contains("request_base64 must be valid base64"));
    }

    #[test]
    fn rejects_both_request_forms_at_once() {
        let yaml = r#"
version: 1
scenarios:
  - name: GrpcMixedRequest
    scenarios_per_sec: 1
    max_concurrency: 1
    duration: 5s
    repeat:
      type: once
    steps:
      - protocol: grpc
        service: account.v1.AccountService
        method: CreateAccount
        request_base64: "CgwrMTIzNDU2Nzg5MDA="
        request_fields:
          phone: "${gen.phone}"
"#;
        let err = match parse_scenarios_yaml(yaml, "http://127.0.0.1:8080") {
            Ok(_) => panic!("must fail schema validation"),
            Err(err) => err,
        };
        assert!(err.contains("only one of request_base64 or request_fields"));
    }
}
