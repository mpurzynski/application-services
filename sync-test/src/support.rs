/* Any copyright is dedicated to the Public Domain.
   http://creativecommons.org/publicdomain/zero/1.0/ */

// This is required to prevent warnings about unused functions in this file just
// because it's unused in a single file (tests that don't use every function in
// this module will get warnings otherwise).
#![allow(dead_code)]

use fxa_client::{self, FirefoxAccount, Config as FxaConfig};
use logins_sql::{Login, PasswordEngine};
use logins_sql::Result as LoginResult;

use url::Url;

use std::env;
use std::path::PathBuf;
use std::collections::HashMap;
use std::sync::{Once, ONCE_INIT, Arc};
use failure;
use serde_json;
use sync15_adapter::{Sync15StorageClientInit, KeyBundle};
use env_logger;

type FailureResult<T> = Result<T, failure::Error>;

pub const CLIENT_ID: &str = "98adfa37698f255b"; // Hrm...
pub const SYNC_SCOPE: &str = "https://identity.mozilla.com/apps/oldsync";

// TODO: This is wrong for dev?
pub const REDIRECT_URI: &str = "https://lockbox.firefox.com/fxa/ios-redirect.html";

lazy_static! {
    // Figures out where `sync-test/helper` lives. This is pretty gross, but once
    // https://github.com/rust-lang/cargo/issues/2841 is resolved it should be simpler.
    // That said, it's possible we should probably just rewrite that script in rust instead :p.
    static ref HELPER_SCRIPT_DIR: PathBuf = {
        let mut path = env::current_exe().expect("Failed to get current exe path...");
        // Find `target` which should contain this program.
        while path.file_name().expect("Failed to find target!") != "target" {
            path.pop();
        }
        // And go up once more, to the root of the workspace.
        path.pop();
        // TODO: it would be nice not to hardcode these given that we're
        // planning on moving stuff around, but such is life.
        path.push("sync-test");
        path.push("helper");
        path
    };
}

fn run_helper_command(cmd: &str, cmd_args: &[&str]) -> Result<String, failure::Error> {
    use std::process::{self, Command};
    // This `Once` is used to run `npm install` first time through.
    static HELPER_SETUP: Once = ONCE_INIT;
    HELPER_SETUP.call_once(|| {
        let dir = &*HELPER_SCRIPT_DIR;
        env::set_current_dir(dir).expect("Failed to change directory...");

        // Let users know why this is happening even if `log` isn't enabled.
        println!("Running `npm install` in `integration-test-helper` to ensure it's usable");

        let mut child = Command::new("npm")
            .args(&["install"])
            .spawn()
            .expect("Failed to spawn `npm install`! (This test currently requires `node`)");

        child.wait()
             .expect("Failed to install helper dependencies, can't run integration test");
    });
    // We should still be in the script dir from HELPER_SETUP's call_once.
    info!("Running helper script with command \"{}\"", cmd);

    // node_args = ["index.js", cmd, ...cmd_args] in JavaScript parlance.
    let node_args: Vec<&str> = ["index.js", cmd]
        .iter()
        .chain(cmd_args.iter())
        .cloned() // &&str -> &str
        .collect();

    let child = Command::new("node")
        .args(&node_args)
        // Grab stdout, but inherit stderr.
        .stdout(process::Stdio::piped())
        .stderr(process::Stdio::inherit())
        .spawn()?;

    let output = child.wait_with_output()?;
    if !output.status.success() {
        let exit_reason = output.status.code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "(process terminated by signal)".to_string());
        // Print stdout in case something helpful was logged there, as well as the exit status
        println!("Helper script exited with {}, it's stdout was:```\n{}\n```",
                 exit_reason, String::from_utf8_lossy(&output.stdout));
        bail!("Failed to run helper script");
    }
    // Note: from_utf8_lossy returns a Cow
    let result = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(result)
}

// It's important that this doesn't implement Clone! (It destroys it's temporary fxaccount on drop)
#[derive(Debug)]
pub struct TestAccount {
    pub email: String,
    pub pass: String,
    pub cfg: FxaConfig,
}

impl TestAccount {
    fn new(email: String, pass: String, cfg: FxaConfig) -> FailureResult<Arc<TestAccount>> {
        info!("Creating temporary fx account");
        // `create` doesn't return anything we care about.
        let auth_url = cfg.auth_url()?;
        run_helper_command("create", &[&email, &pass, auth_url.as_str()])?;
        Ok(Arc::new(TestAccount { email, pass, cfg }))
    }

    pub fn new_random() -> FailureResult<Arc<TestAccount>> {
        use rand::{self, prelude::*};
        let mut rng = thread_rng();
        let name = format!("rust-login-sql-test--{}",
            rng.sample_iter(&rand::distributions::Alphanumeric).take(5).collect::<String>());
        // Just use the username for the password in case we need to clean these
        // up easily later because of some issue.
        let password = name.clone();
        let email = format!("{}@restmail.net", name);
        Self::new(email, password, FxaConfig::stable_dev()?)
    }
}

impl Drop for TestAccount {
    fn drop(&mut self) {
        info!("Cleaning up temporary firefox account");
        let auth_url = self.cfg.auth_url().unwrap(); // We already parsed this once.
        if let Err(e) = run_helper_command("destroy", &[&self.email, &self.pass, auth_url.as_str()]) {
            warn!("Failed to destroy fxacct {} with pass {}!", self.email, self.pass);
            warn!("   Error: {}", e);
        }
    }
}

#[derive(Debug, Deserialize)]
struct ScopedKeyData {
    k: String,
    kty: String,
    kid: String,
    scope: String,
}

pub struct TestClient {
    pub fxa: fxa_client::FirefoxAccount,
    pub test_acct: Arc<TestAccount>,
    pub engine: PasswordEngine,
}

impl TestClient {
    pub fn new(acct: Arc<TestAccount>) -> FailureResult<Self> {
        info!("Doing oauth flow!");

        let mut fxa = FirefoxAccount::new(acct.cfg.clone(), CLIENT_ID, REDIRECT_URI);
        let oauth_uri = fxa.begin_oauth_flow(&[SYNC_SCOPE], true)?;
        let auth_url = acct.cfg.auth_url()?;
        let redirected_to = run_helper_command("oauth", &[
            &acct.email, &acct.pass, auth_url.as_str(), &oauth_uri
        ])?;

        let final_url = Url::parse(&redirected_to)?;
        let query_params = final_url.query_pairs().into_owned().collect::<HashMap<String, String>>();

        // should we be using the OAuthInfo this returns?
        fxa.complete_oauth_flow(&query_params["code"], &query_params["state"])?;
        info!("OAuth flow finished");

        Ok(Self {
            fxa,
            test_acct: acct,
            engine: PasswordEngine::new_in_memory(None)?,
        })
    }

    pub fn data_for_sync(&mut self) -> FailureResult<(Sync15StorageClientInit, KeyBundle)> {
        // Allow overriding it via environment
        let tokenserver_url = option_env!("TOKENSERVER_URL").map(|env_var| {
            // We hard error here even though we want to return a Result to provide a clearer
            // error for misconfiguration
            Ok(Url::parse(env_var).expect("Failed to parse TOKENSERVER_URL environment variable!"))
        }).unwrap_or_else(|| {
            self.test_acct.cfg.token_server_endpoint_url()
        })?;

        let token = self.fxa.get_oauth_token(&[SYNC_SCOPE])?.unwrap();

        let keys: HashMap<String, ScopedKeyData> = serde_json::from_str(&token.keys.unwrap())?;
        let key = keys.get(SYNC_SCOPE).unwrap();

        let client_init = Sync15StorageClientInit {
            key_id: key.kid.clone(),
            access_token: token.access_token.clone(),
            tokenserver_url,
        };

        let root_sync_key = KeyBundle::from_ksync_base64(&key.k)?;

        Ok((client_init, root_sync_key))
    }

    pub fn fully_wipe_server(&self) -> FailureResult<bool> {
        use sync15_adapter::client::SetupStorageClient;
        match self.engine.get_sync_info() {
            Some(info) => {
                info.client.wipe_all_remote()?;
                Ok(true)
            },
            None => {
                Ok(false)
            }
        }
    }

    pub fn fully_reset_local_db(&mut self) -> FailureResult<()> {
        self.engine = PasswordEngine::new_in_memory(None)?;
        Ok(())
    }

    pub fn sync(&mut self) -> FailureResult<()> {
        let (init, key) = self.data_for_sync()?;
        self.engine.sync(&init, &key)?;
        Ok(())
    }

}

// Wipes the server using the first client that can manage it.
// We do this at the end of each test to avoid creating N accounts for N tests,
// and just creating 1 account per file containing tests.
pub fn cleanup_server(clients: &[&TestClient]) -> FailureResult<()> {
    info!("Cleaning up server after tests...");
    for c in clients {
        match c.fully_wipe_server() {
            Ok(true) => {
                return Ok(())
            },
            Ok(false) => {}
            Err(e) => {
                warn!("Error when wiping server: {:?}", e);
            }
        }
    }
    bail!("None of the clients managed to wipe the server!");
}

pub fn init_test_logging() {
    static LOG_INIT: Once = ONCE_INIT;
    LOG_INIT.call_once(|| {
        env_logger::init_from_env(
            env_logger::Env::default().filter_or("RUST_LOG",
                "trace,tokio_threadpool=warn,tokio_reactor=warn,tokio_core=warn,tokio=warn,hyper=warn,want=warn,mio=warn,reqwest=warn")
        );
    });
}

// Doesn't check metadata fields
pub fn assert_logins_equiv(a: &Login, b: &Login) {
    assert_eq!(b.id, a.id, "id mismatch");
    assert_eq!(b.hostname, a.hostname, "hostname mismatch");
    assert_eq!(b.form_submit_url, a.form_submit_url, "form_submit_url mismatch");
    assert_eq!(b.http_realm, a.http_realm, "http_realm mismatch");
    assert_eq!(b.username, a.username, "username mismatch");
    assert_eq!(b.password, a.password, "password mismatch");
    assert_eq!(b.username_field, a.username_field, "username_field mismatch");
    assert_eq!(b.password_field, a.password_field, "password_field mismatch");
}

pub fn times_used_for_id(e: &PasswordEngine, id: &str) -> i64 {
    e.get(id).expect("get() failed").expect("Login doesn't exist").times_used
}

pub fn add_login(e: &PasswordEngine, l: Login) -> LoginResult<Login> {
    let id = e.add(l)?;
    Ok(e.get(&id)?.expect("Login we just added to exist"))
}

pub fn verify_login(e: &PasswordEngine, l: &Login) {
    let equivalent = e.get(&l.id)
        .expect("get() to succeed")
        .expect("Expected login to be present");
    assert_logins_equiv(&equivalent, l);
}

pub fn verify_missing_login(e: &PasswordEngine, id: &str) {
    assert!(e.get(id).expect("get() to succeed").is_none(), "Login {} should not exist", id);
}

pub fn update_login<F: FnMut(&mut Login)>(e: &PasswordEngine, id: &str, mut callback: F) -> LoginResult<Login> {
    let mut login = e.get(id)?.expect("No such login!");
    callback(&mut login);
    e.update(login)?;
    Ok(e.get(id)?.expect("Just updated this"))
}

pub fn touch_login(e: &PasswordEngine, id: &str, times: usize) -> LoginResult<Login> {
    for _ in 0..times {
        e.touch(&id)?;
    }
    Ok(e.get(&id)?.unwrap())
}

