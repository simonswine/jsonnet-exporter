use bytes::Buf;
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
use warp::{http::header::HeaderValue, http::header::CONTENT_TYPE, http::Response, Filter};

use prometheus::{labels, opts, register_counter, register_gauge, register_histogram_vec};
use prometheus::{Counter, Encoder, Gauge, HistogramVec, TextEncoder};

use jrsonnet_evaluator::{
    native::NativeCallback, throw, EvaluationState, FileImportResolver, ImportResolver, Val,
};
use jrsonnet_interner::IStr;
use jrsonnet_parser::{Param, ParamsDesc};

use lazy_static::lazy_static;

use regex::Regex;

lazy_static! {
    static ref HTTP_COUNTER: Counter = register_counter!(opts!(
        "example_http_requests_total",
        "Number of HTTP requests made.",
        labels! {"handler" => "all",}
    ))
    .unwrap();
    static ref HTTP_BODY_GAUGE: Gauge = register_gauge!(opts!(
        "example_http_response_size_bytes",
        "The HTTP response sizes in bytes.",
        labels! {"handler" => "all",}
    ))
    .unwrap();
    static ref HTTP_REQ_HISTOGRAM: HistogramVec = register_histogram_vec!(
        "example_http_request_duration_seconds",
        "The HTTP request latencies in seconds.",
        &["handler"]
    )
    .unwrap();
}

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

// TODO: Define error better
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
    fn state(&self) -> Result<Module> {
        let state = EvaluationState::default();
        state.with_stdlib();

        let cb = Rc::new(NativeCallback::new(
            ParamsDesc(Rc::new(vec![
                Param("regex".into(), None),
                Param("string".into(), None),
            ])),
            |_caller, args| match (&args[0], &args[1]) {
                (Val::Str(regex), Val::Str(string)) => {
                    let re = Regex::new(regex).unwrap();

                    let matches: Vec<Val> = re
                        .captures_iter(string)
                        .into_iter()
                        .map(|capture| {
                            let val: Vec<Val> = capture
                                .iter()
                                .filter_map(|submatch| match submatch {
                                    Some(m) => Some(Val::Str(m.as_str().into())),
                                    None => Some(Val::Null),
                                })
                                .collect();
                            Val::Arr(val.into())
                        })
                        .collect();

                    debug!("native call regexMatch={:?}", matches);

                    Ok(Val::Arr(matches.into()))
                }
                (_, _) => unreachable!(),
            },
        ));

        state.add_native("regexMatch".into(), cb);
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

        Ok(Module {
            state: state,
            path: path,
        })
    }

    fn validate(&self) -> Result<()> {
        // TODO        state.set_manifest_format(jrsonnet_evaluator::ManifestFormat::Json(3));
        let module = self.state()?;

        // TODO move into subcommand
        if let Some(tests) = &self.tests {
            for test in tests.iter() {
                info!("test: {:?}", test);
                let actual = module.eval(&test.input)?;

                if actual == test.output {
                    debug!("test of module TODO.# passed")
                } else {
                    let actual_lines = actual.split("\n").collect::<Vec<&str>>();
                    let expected_lines = test.output.split("\n").collect::<Vec<&str>>();
                    error!(
                        "test of module TODO.# failed:\n\
                  {}\n\
                  ",
                        Comparison::new(&actual_lines, &expected_lines)
                    );
                }
            }
        };

        //

        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct InputData {
    body: serde_json::Value,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct ConfigModuleTest {
    input: String,
    output: String,
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

use warp::{Rejection, Reply};

#[derive(Debug)]
struct MissingQueryParameter {
    name: String,
}

#[derive(Debug)]
enum ProbeError {
    MissingParameter(String),
    ModuleNotFound(String),
    InvalidTargetUrl(warp::http::uri::InvalidUri),
    TargetHTTP(hyper::Error),
    TargetJSONParse(serde_json::Error),
}

impl warp::reject::Reject for MissingQueryParameter {}

impl warp::reject::Reject for ProbeError {}

async fn metrics_handler() -> std::result::Result<impl Reply, Rejection> {
    let encoder = TextEncoder::new();

    HTTP_COUNTER.inc();
    let timer = HTTP_REQ_HISTOGRAM.with_label_values(&["all"]).start_timer();

    let metric_families = prometheus::gather();
    let mut buffer = vec![];
    encoder.encode(&metric_families, &mut buffer).unwrap();
    HTTP_BODY_GAUGE.set(buffer.len() as f64);

    let response = Response::builder()
        .status(200)
        .header(CONTENT_TYPE, encoder.format_type())
        .body(buffer)
        .unwrap();

    timer.observe_duration();

    Ok(response)
}

struct Module {
    path: Rc<PathBuf>,
    state: EvaluationState,
}

impl Module {
    fn eval(&self, input: &String) -> Result<String> {
        let eval = format!(
            r#"
local s = import '{}';

s.process(std.extVar("input"))
"#,
            self.path.to_path_buf().to_str().expect("unpack string")
        );

        self.state
            .add_ext_code("input".into(), input.clone().into())
            .map_err(|e| match e {
                e => format!("err {:?}", e),
            })?;

        let path = Rc::new(PathBuf::from("eval.jsonnet"));
        let result = self
            .state
            .evaluate_snippet_raw(path, eval.into())
            .map_err(|e| match e {
                e => format!("err {:?}", e),
            })?;
        info!("result = {:?}", result);

        let manifest = self.state.manifest(result).map_err(|e| match e {
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

        Ok(String::from_utf8(buffer).unwrap())
    }
}

struct App {
    config: Config,
    opts: Opts,
}

impl App {
    fn new() -> Self {
        let opts: Opts = Opts::parse();

        // Setup logger with default level info so we can see the messages from
        // prometheus_exporter.
        Builder::from_env(Env::default().default_filter_or("info")).init();

        // Parse config file
        let config_file = File::open(&opts.config_file).expect("cannot open config file");
        let config_reader = BufReader::new(config_file);
        let config: Config =
            serde_yaml::from_reader(config_reader).expect("cannot parse config file");
        debug!("read config {:?}", config);

        App {
            config: config,
            opts: opts,
        }
    }
    async fn probe_handler(
        &self,
        params: HashMap<String, String>,
    ) -> std::result::Result<impl Reply, Rejection> {
        let module_name = match params.get("module") {
            Some(module_name) => module_name,
            None => {
                return Err(warp::reject::custom(ProbeError::MissingParameter(
                    "module".into(),
                )));
            }
        };

        let module = match self.config.modules.get(module_name) {
            Some(m) => m,
            None => {
                return Err(warp::reject::custom(ProbeError::ModuleNotFound(
                    module_name.clone(),
                )))
            }
        };

        let target = match params.get("target") {
            Some(target) => target,
            None => {
                return Err(warp::reject::custom(ProbeError::MissingParameter(
                    "target".into(),
                )));
            }
        };

        let uri = target
            .parse()
            .map_err(|e| ProbeError::InvalidTargetUrl(e))?;

        // Await the response...
        use hyper::Client;
        use hyper_tls::HttpsConnector;
        let https = HttpsConnector::new();
        let client = Client::builder().build::<_, hyper::Body>(https);
        let resp = client
            .get(uri)
            .await
            .map_err(|e| ProbeError::TargetHTTP(e))?;
        let headers = &resp.headers().clone();

        let body = hyper::body::aggregate(resp)
            .await
            .map_err(|e| ProbeError::TargetHTTP(e))?;

        let json_body: serde_json::Value = match headers.get(CONTENT_TYPE) {
            Some(header_value) if header_value == HeaderValue::from_static("application/json") => {
                info!("json response");
                serde_json::from_reader(body.reader())
                    .map_err(|e| ProbeError::TargetJSONParse(e))?
            }
            _ => {
                info!("string response");
                let mut buffer = String::new();
                body.reader().read_to_string(&mut buffer).unwrap();
                serde_json::Value::String(buffer)
            }
        };

        let data = serde_json::to_string(&InputData { body: json_body }).unwrap();

        info!("{:?}", data);

        let metrics = module.state().unwrap().eval(&data).unwrap();

        Ok(metrics)
    }
}

lazy_static! {
    static ref APP: App = App::new();
}

#[tokio::main]
async fn main() {
    APP.config.validate().expect("cannot validate config file");

    // GET /hello/warp => 200 OK with body "Hello, warp!"
    let hello = warp::path!("hello" / String).map(|name| format!("Hello, {}!", name));

    let metrics = warp::path!("metrics").and_then(metrics_handler);

    let probe = warp::path!("probe")
        .and(warp::query::<HashMap<String, String>>())
        .and_then(|p| APP.probe_handler(p));

    let routes = warp::get().and(hello.or(metrics).or(probe));
    // Parse address used to bind exporter to.
    let addr: SocketAddr = APP
        .opts
        .bind_addr
        .parse()
        .expect("can not parse listen addr");

    warp::serve(routes).run(addr).await;
}
