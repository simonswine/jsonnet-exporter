extern crate env_logger;
extern crate futures;
extern crate log;
extern crate prometheus_exporter;
extern crate tokio;

use clap::Clap;
use env_logger::{Builder, Env};
use log::{debug, error, info};
use pretty_assertions::Comparison;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::fs::File;
use std::io::BufReader;
use std::net::SocketAddr;
use std::{any::Any, io::prelude::*, path::PathBuf, rc::Rc};

use jrsonnet_evaluator::{throw, EvaluationState, FileImportResolver, ImportResolver};
use jrsonnet_interner::IStr;

#[derive(Clap)]
#[clap(author = "Christian Simon <simon@swine.de>")]
struct Opts {
    /// The port the exporter listens to.
    #[clap(long = "bind-addr", default_value = "0.0.0.0:9186")]
    bind_addr: String,

    /// The path to the config file.
    #[clap(long = "config-file", default_value = "config.yaml")]
    config_file: String,

    /// Library search dirs.
    /// Any not found `imported` file will be searched in these.
    /// This can also be specified via `JSONNET_PATH` variable,
    /// which should contain a colon-separated (semicolon-separated on Windows) list of directories.
    #[clap(long, short = 'J')]
    _jpath: Vec<PathBuf>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Config {
    modules: HashMap<String, ConfigModule>,
}

type Result<T> = std::result::Result<T, Box<dyn Error>>;

impl Config {
    fn validate(&self) -> Result<()> {
        for (name, module) in &self.modules {
            module
                .validate()
                .map_err(|e| format!("module '{}' {:?}", name, e))?;
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct ConfigModule {
    jsonnet_path: Option<String>,
    jsonnet: Option<String>,
    tests: Option<Vec<ConfigModuleTest>>,
}

// MemoryImportResolver allows to import a single other file from memory
#[derive(Debug)]
struct MemoryImportResolver {
    path: PathBuf,
    code: IStr,
}

impl ImportResolver for MemoryImportResolver {
    fn resolve_file(
        &self,
        from: &PathBuf,
        path: &PathBuf,
    ) -> jrsonnet_evaluator::error::Result<Rc<PathBuf>> {
        if path == &self.path {
            return Ok(Rc::new(self.path.clone()));
        }
        throw!(jrsonnet_evaluator::error::Error::ImportFileNotFound(
            from.clone(),
            path.clone()
        ))
    }

    fn load_file_contents(&self, _resolved: &PathBuf) -> jrsonnet_evaluator::error::Result<IStr> {
        // Can be only caused by library direct consumer, not by supplied jsonnet
        panic!("dummy resolver can't load any file")
    }

    unsafe fn as_any(&self) -> &dyn Any {
        panic!("`as_any($self)` is not supported by dummy resolver")
    }
}

impl ConfigModule {
    fn validate(&self) -> Result<()> {
        let state = EvaluationState::default();
        state.with_stdlib();

        //
        state.set_manifest_format(jrsonnet_evaluator::ManifestFormat::Json(3));

        let (path, jsonnet) = match (&self.jsonnet, &self.jsonnet_path) {
            (Some(_), Some(_)) => Err("Only one of 'jsonnet' and 'jsonnet_path' can be set"),
            (None, None) => Err("One of 'jsonnet' or 'jsonnet_path' has to be set"),
            (Some(jsonnet), None) => {
                let path = PathBuf::from("inline.jsonnet");

                // only allow that single snippet being exported
                state.set_import_resolver(Box::new(MemoryImportResolver {
                    path: path.clone(),
                    code: jsonnet.to_owned().into(),
                }));

                Ok((Rc::new(PathBuf::from(path)), jsonnet.to_owned().into()))
            }
            (None, Some(jsonnet_file)) => {
                let path = Rc::new(PathBuf::from(jsonnet_file));
                let mut file = File::open(jsonnet_file)?;
                let mut out = String::new();
                file.read_to_string(&mut out)?;

                // TODO import differently configured _jpath
                state.set_import_resolver(Box::new(FileImportResolver {
                    library_paths: vec![],
                }));

                Ok((path.clone(), out.into()))
            }
        }?;

        state.add_file(path.clone(), jsonnet).map_err(|e| match e {
            e => format!("err {:?}", e),
        })?;

        // TODO move into subcommand
        if let Some(tests) = &self.tests {
            for test in tests.iter() {
                info!("test: {:?}", test);
                test.run(&state, &path)?;
            }
        };

        //

        Ok(())
    }
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct ConfigModuleTest {
    input: String,
    output: String,
}

impl ConfigModuleTest {
    fn run(&self, state: &jrsonnet_evaluator::EvaluationState, path: &Rc<PathBuf>) -> Result<()> {
        let eval = format!(
            r#"
local s = import '{}';

s.process(std.extVar("input"))
"#,
            path.to_path_buf().to_str().expect("unpack string")
        );

        state
            .add_ext_code("input".into(), self.input.clone().into())
            .map_err(|e| match e {
                e => format!("err {:?}", e),
            })?;

        let path = Rc::new(PathBuf::from("eval.jsonnet"));
        let result = state
            .evaluate_snippet_raw(path, eval.into())
            .map_err(|e| match e {
                e => format!("err {:?}", e),
            })?;
        info!("result = {:?}", result);

        let manifest = state.manifest(result).map_err(|e| match e {
            e => format!("err {:?}", e),
        })?;

        let metrics: Metrics = serde_json::from_str(&manifest)?;

        let registry = prometheus::Registry::new();

        for (metric_name, metric) in metrics.0 {
            let m = match metric.r#type {
                MetricType::Gauge => {
                    let label_names = match &metric.label_names {
                        Some(ln) => ln.iter().map(std::ops::Deref::deref).collect(),
                        None => vec![],
                    };
                    let opts = prometheus::Opts::new(
                        metric_name,
                        match &metric.help {
                            Some(help) => help,
                        _ => "jsonnet-exporter: Metric help is missing, consider adding a help text to the module config.",
                        },
                    );
                    let m = prometheus::GaugeVec::new(opts, &label_names)?;
                    m
                }
            };
            registry.register(Box::new(m.clone()))?;

            for s in &metric.series {
                let label_values = match &s.label_values {
                    Some(lv) => lv.iter().map(std::ops::Deref::deref).collect(),
                    None => vec![],
                };
                m.with_label_values(&label_values).set(s.value);
            }
        }

        // Gather the metrics.
        let mut buffer = vec![];
        let encoder = prometheus::TextEncoder::new();
        let metric_families = registry.gather();
        encoder.encode(&metric_families, &mut buffer)?;
        let actual = String::from_utf8(buffer).unwrap();

        if actual == self.output {
            debug!("test of module TODO.# passed")
        } else {
            let actual_lines = actual.split("\n").collect::<Vec<&str>>();
            let expected_lines = self.output.split("\n").collect::<Vec<&str>>();
            error!(
                "test of module TODO.# failed:\n\
                  {}\n\
                  ",
                Comparison::new(&actual_lines, &expected_lines)
            );
        }

        Ok(())
    }
}

#[derive(serde::Deserialize, Debug)]
enum MetricType {
    #[serde(rename = "gauge")]
    Gauge,
}

#[derive(serde::Deserialize, Debug)]
struct Metric {
    label_names: Option<Vec<String>>,
    #[serde(default)]
    series: Vec<Series>,
    help: Option<String>,
    r#type: MetricType,
}

#[derive(serde::Deserialize, Debug)]
struct Metrics(HashMap<String, Metric>);

use prometheus::Encoder;
use prometheus_exporter::prometheus;

#[derive(serde::Deserialize, Debug)]
struct Series {
    label_values: Option<Vec<String>>,
    value: f64,
}

fn iterate(key: Option<String>, value: &serde_json::Value) {
    use serde_json::Value::*;
    match value {
        Null | Bool(_) | String(_) | Number(_) => {}
        Array(a) => {
            for value in a {
                iterate(key.clone(), value)
            }
        }
        Object(o) => {
            let mut is_leaf = true;
            for (key, value) in o {
                // skip labels as it's the only allowed leaf object
                if key == "labels" {
                    continue;
                }

                // determine if children are nested arrays or objects,
                match value {
                    Array(_) | Object(_) => {
                        is_leaf = false;
                    }
                    _ => {}
                };
                iterate(Some(key.clone()), &value);
            }
            if is_leaf {
                info!("leaf node: {:?}", o);
            }
        }
    }
}

fn main() {
    let opts: Opts = Opts::parse();

    // Setup logger with default level info so we can see the messages from
    // prometheus_exporter.
    Builder::from_env(Env::default().default_filter_or("info")).init();

    // Parse config file
    let config_file = File::open(opts.config_file).expect("cannot open config file");
    let config_reader = BufReader::new(config_file);
    let config: Config = serde_yaml::from_reader(config_reader).expect("cannot parse config file");
    debug!("read config {:?}", config);

    config.validate().expect("cannot validate config file");

    // Parse address used to bind exporter to.
    let addr: SocketAddr = opts.bind_addr.parse().expect("can not parse listen addr");

    // Start exporter
    let exporter = prometheus_exporter::start(addr).expect("can not start exporter");

    loop {
        let _guard = exporter.wait_request();
    }
}
