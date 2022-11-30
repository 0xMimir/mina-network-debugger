mod routes;
mod database;
use self::database::{Client, Config, Database};

use std::{thread, env, sync::{Arc, atomic::{Ordering, AtomicBool}}, fs::File, io::Read, time::Duration};

use tokio::{sync::oneshot, runtime::Runtime};

fn main() {
    env_logger::init();

    let key_path = env::var("HTTPS_KEY_PATH").ok();
    let cert_path = env::var("HTTPS_CERT_PATH").ok();
    let port = env::var("SERVER_PORT")
        .unwrap_or_else(|_| 8000.to_string())
        .parse()
        .unwrap_or(8000);

    let rt = match Runtime::new() {
        Ok(v) => v,
        Err(err) => {
            log::error!("fatal: {err}");
            return;
        }
    };

    let database = Database::default();

    let _guard = rt.enter();
    let (tx, rx) = oneshot::channel();
    let addr = ([0, 0, 0, 0], port);
    let routes = routes::routes(database.clone());
    let shutdown = async move {
        rx.await.expect("corresponding sender should exist");
        log::info!("terminating http server...");
    };
    let server_thread = if let (Some(key_path), Some(cert_path)) = (key_path, cert_path) {
        let (_, server) = warp::serve(routes)
            .tls()
            .key_path(key_path)
            .cert_path(cert_path)
            .bind_with_graceful_shutdown(addr, shutdown);
        thread::spawn(move || rt.block_on(server))
    } else {
        let (_, server) = warp::serve(routes).bind_with_graceful_shutdown(addr, shutdown);
        thread::spawn(move || rt.block_on(server))
    };
    let callback = move || tx.send(()).expect("corresponding receiver should exist");

    let terminating = Arc::new(AtomicBool::new(false));
    {
        let terminating = terminating.clone();
        let mut callback = Some(callback);
        let user_handler = move || {
            log::info!("ctrlc");
            if let Some(cb) = callback.take() {
                cb();
            }
            terminating.store(true, Ordering::SeqCst);
        };
        if let Err(err) = ctrlc::set_handler(user_handler) {
            log::error!("failed to set ctrlc handler {err}");
            return;
        }
    }

    let mut s = String::new();
    let mut f = File::open("config.ron").unwrap();
    f.read_to_string(&mut s).unwrap();
    let config = ron::from_str::<Config>(&s).unwrap();
    let client = Client::new(config);
    
    'main: while !terminating.load(Ordering::SeqCst) {
        client.refresh(&database);

        for _ in 0..10 {
            thread::sleep(Duration::from_secs(1));
            if terminating.load(Ordering::SeqCst) {
                break 'main;
            }
        }
    }

    if server_thread.join().is_err() {
        log::error!("server thread panic, this is a bug, must not happen");
    }
    log::info!("terminated");
}
