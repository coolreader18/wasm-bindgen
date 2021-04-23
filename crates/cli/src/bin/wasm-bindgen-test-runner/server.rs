use std::ffi::OsString;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Error};
use tiny_http::{Request, Response};
use url::Url;

pub struct Server {
    server: tiny_http::Server,
    handler: Arc<Handler>,
}

impl Server {
    pub fn run(self) {
        for req in self.server.incoming_requests() {
            let handler = self.handler.clone();
            std::thread::spawn(move || handler.process_request(req));
        }
    }

    pub fn server_addr(&self) -> SocketAddr {
        self.server.server_addr()
    }
}

struct Handler {
    tmpdir: PathBuf,
    headless: bool,
}

impl Handler {
    fn process_request(&self, request: Request) {
        let e404 = || Response::empty(404);
        let url = match Url::parse(request.url()) {
            Ok(u) => u,
            Err(_) => {
                let _ = request.respond(e404());
                return;
            }
        };
        // The root path gets our canned `index.html`. The two templates here
        // differ slightly in the default routing of `console.log`, going to an
        // HTML element during headless testing so we can try to scrape its
        // output.
        if request.url() == "/" {
            let s = if self.headless {
                include_str!("index-headless.html")
            } else {
                include_str!("index.html")
            };
            let res = Response::from_data(s).with_header(mime("index.html".as_ref()));
            let _ = request.respond(res);
            return;
        }

        // Otherwise we need to find the asset here. It may either be in our
        // temporary directory (generated files) or in the main directory
        // (relative import paths to JS). Try to find both locations.
        let file_response = try_asset(&url, &self.tmpdir).or_else(|| try_asset(&url, ".".as_ref()));
        match file_response {
            Some(response) => {
                let _ = request.respond(response);
            }
            None => {
                let _ = request.respond(e404());
            }
        }
    }
}

fn mime(p: &Path) -> tiny_http::Header {
    let mime = match p.extension().and_then(|s| s.to_str()) {
        Some("js") => "text/javascript",
        Some("wasm") => "application/wasm",
        Some("html") => "text/html",
        _ => "application/octet-stream",
    };
    tiny_http::Header::from_bytes("Content-Type".as_bytes(), mime.as_bytes()).unwrap()
}

fn try_asset(url: &Url, dir: &Path) -> Option<Response<fs::File>> {
    let mut full_path = dir.join(url.path().strip_prefix('/')?);
    if let Ok(f) = fs::File::open(&full_path) {
        return Some(Response::from_file(f).with_header(mime(&full_path)));
    }

    // When a browser is doing ES imports it's using the directives we
    // write in the code that *don't* have file extensions (aka we say `from
    // 'foo'` instead of `from 'foo.js'`. Fixup those paths here to see if a
    // `js` file exists.
    if full_path.extension().is_none() {
        full_path.set_extension("js");
        if let Ok(f) = fs::File::open(&full_path) {
            return Some(Response::from_file(f).with_header(mime(&full_path)));
        }
    }
    None
}

pub fn spawn(
    addr: &SocketAddr,
    headless: bool,
    module: &str,
    tmpdir: &Path,
    args: &[OsString],
    tests: &[String],
) -> Result<Server, Error> {
    let mut js_to_execute = format!(
        r#"
        import {{
            WasmBindgenTestContext as Context,
            __wbgtest_console_debug,
            __wbgtest_console_log,
            __wbgtest_console_info,
            __wbgtest_console_warn,
            __wbgtest_console_error,
            default as init,
        }} from './{0}';

        // Now that we've gotten to the point where JS is executing, update our
        // status text as at this point we should be asynchronously fetching the
        // wasm module.
        document.getElementById('output').textContent = "Loading wasm module...";

        async function main(test) {{
            const wasm = await init('./{0}_bg.wasm');

            const cx = new Context();
            window.on_console_debug = __wbgtest_console_debug;
            window.on_console_log = __wbgtest_console_log;
            window.on_console_info = __wbgtest_console_info;
            window.on_console_warn = __wbgtest_console_warn;
            window.on_console_error = __wbgtest_console_error;

            // Forward runtime arguments. These arguments are also arguments to the
            // `wasm-bindgen-test-runner` which forwards them to node which we
            // forward to the test harness. this is basically only used for test
            // filters for now.
            cx.args({1:?});

            await cx.run(test.map(s => wasm[s]));
        }}

        const tests = [];
    "#,
        module, args,
    );
    for test in tests {
        js_to_execute.push_str(&format!("tests.push('{}');\n", test));
    }
    js_to_execute.push_str("main(tests);\n");

    let js_path = tmpdir.join("run.js");
    fs::write(&js_path, js_to_execute).context("failed to write JS file")?;

    // For now, always run forever on this port. We may update this later!
    let tmpdir = tmpdir.to_path_buf();
    let server = tiny_http::Server::http(addr).map_err(|e| anyhow!(e))?;
    let handler = Arc::new(Handler { tmpdir, headless });
    Ok(Server { server, handler })
}
