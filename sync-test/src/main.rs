/* Any copyright is dedicated to the Public Domain.
   http://creativecommons.org/publicdomain/zero/1.0/ */

extern crate logins_sql;
extern crate sync15_adapter;
extern crate fxa_client;
extern crate url;

extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;

extern crate env_logger;

#[macro_use]
extern crate log;

#[macro_use]
extern crate failure;
extern crate rand;

#[macro_use]
extern crate lazy_static;

mod support;

use support::*;

use logins_sql::Login;

macro_rules! cleanup_clients {
    ($($client:ident),+) => {
        info!("Cleaning up!");
        cleanup_server(&[$((&$client)),+]).expect("Remote cleanup failed");
        // rebind as mut, do call, rebind as const
        $($client.fully_reset_local_db().expect("Failed to reset client");)+
    };
}

fn test_login_general(c0: &mut TestClient, c1: &mut TestClient) {
    info!("Add some logins to client0");

    let l0id = "aaaaaaaaaaaa";
    let l1id = "bbbbbbbbbbbb";

    add_login(&c0.engine, Login {
        id: l0id.into(),
        hostname: "http://www.example.com".into(),
        form_submit_url: Some("http://login.example.com".into()),
        username: "cool_username".into(),
        password: "hunter2".into(),
        username_field: "uname".into(),
        password_field: "pword".into(),
        .. Login::default()
    }).expect("add l0");

    let login0_c0 = touch_login(&c0.engine, l0id, 2).expect("touch0 c0");
    assert_eq!(login0_c0.times_used, 3);

    let login1_c0 = add_login(&c0.engine, Login {
        id: l1id.into(),
        hostname: "http://www.example.com".into(),
        http_realm: Some("Login".into()),
        username: "cool_username".into(),
        password: "sekret".into(),
        .. Login::default()
    }).expect("add l1");

    info!("Syncing client0");
    c0.sync().expect("c0 sync to work");

    // Should be the same after syncing.
    verify_login(&c0.engine, &login0_c0);
    verify_login(&c0.engine, &login1_c0);

    info!("Syncing client1");
    c1.sync().expect("c1 sync to work");

    info!("Check state");

    verify_login(&c1.engine, &login0_c0);
    verify_login(&c1.engine, &login1_c0);

    assert_eq!(times_used_for_id(&c1.engine, l0id), 3,
               "Times used is wrong (first sync)");

    info!("Update logins");

    // Change login0 on both
    update_login(&c1.engine, l0id, |l| {
        l.password = "testtesttest".into();
    }).unwrap();

    let login0_c0 = update_login(&c0.engine, l0id, |l| {
        l.username_field = "users_name".into();
    }).unwrap();

    // and login1 on remote.
    let login1_c1 = update_login(&c1.engine, l1id, |l| {
        l.username = "less_cool_username".into();
    }).unwrap();

    info!("Sync again");

    c1.sync().expect("c1 sync 2");
    c0.sync().expect("c0 sync 2");

    info!("Check state again");

    // Ensure the remotely changed password change made it through
    verify_login(&c0.engine, &login1_c1);

    // And that the conflicting one did too.
    verify_login(&c0.engine, &Login {
        username_field: "users_name".into(),
        password: "testtesttest".into(),
        ..login0_c0.clone()
    });

    assert_eq!(
        c0.engine.get(l0id).unwrap().unwrap().times_used,
        5, // initially 1, touched twice, updated twice (on two accounts!
           // doing this right requires 3WM)
        "Times used is wrong (final)"
    );
}

fn test_login_deletes(c0: &mut TestClient, c1: &mut TestClient) {
    info!("Add some logins to client0");

    let l0id = "aaaaaaaaaaaa";
    let l1id = "bbbbbbbbbbbb";
    let l2id = "cccccccccccc";
    let l3id = "dddddddddddd";

    let login0 = add_login(&c0.engine, Login {
        id: l0id.into(),
        hostname: "http://www.example.com".into(),
        form_submit_url: Some("http://login.example.com".into()),
        username: "cool_username".into(),
        password: "hunter2".into(),
        username_field: "uname".into(),
        password_field: "pword".into(),
        .. Login::default()
    }).expect("add l0");

    let login1 = add_login(&c0.engine, Login {
        id: l1id.into(),
        hostname: "http://www.example.com".into(),
        http_realm: Some("Login".into()),
        username: "cool_username".into(),
        password: "sekret".into(),
        .. Login::default()
    }).expect("add l1");

    let login2 = add_login(&c0.engine, Login {
        id: l2id.into(),
        hostname: "https://www.example.org".into(),
        http_realm: Some("Test".into()),
        username: "cool_username100".into(),
        password: "123454321".into(),
        .. Login::default()
    }).expect("add l2");

    let login3 = add_login(&c0.engine, Login {
        id: l3id.into(),
        hostname: "https://www.example.net".into(),
        http_realm: Some("Http Realm".into()),
        username: "cool_username99".into(),
        password: "aaaaa".into(),
        .. Login::default()
    }).expect("add l3");

    info!("Syncing client0");

    c0.sync().expect("c0 sync to work");

    // Should be the same after syncing.
    verify_login(&c0.engine, &login0);
    verify_login(&c0.engine, &login1);
    verify_login(&c0.engine, &login2);
    verify_login(&c0.engine, &login3);

    info!("Syncing client1");
    c1.sync().expect("c1 sync to work");

    info!("Check state");
    verify_login(&c1.engine, &login0);
    verify_login(&c1.engine, &login1);
    verify_login(&c1.engine, &login2);
    verify_login(&c1.engine, &login3);

    // The 4 logins are for the for possible scenarios. All of them should result in the record
    // being deleted.

    // 1. Client A deletes record, client B has no changes (should delete).
    // 2. Client A deletes record, client B has also deleted record (should delete).
    // 3. Client A deletes record, client B has modified record locally (should delete).
    // 4. Same as #3 but in reverse order.

    // case 1. (c1 deletes record, c0 should have deleted on the other side)
    info!("Deleting {} from c1", l0id);
    assert!(c1.engine.delete(l0id).expect("Delete should work"));
    verify_missing_login(&c1.engine, l0id);

    // case 2. Both delete l1 separately
    info!("Deleting {} from both", l1id);
    assert!(c0.engine.delete(l1id).expect("Delete should work"));
    assert!(c1.engine.delete(l1id).expect("Delete should work"));

    // case 3a. c0 modifies record (c1 will delete it after c0 syncs so the timestamps line up)
    info!("Updating {} on c0", l2id);
    let login2_new = update_login(&c0.engine, l2id, |l| {
        l.username = "foobar".into();
    }).unwrap();


    // case 4a. c1 deletes record (c0 will modify it after c1 syncs so the timestamps line up)
    assert!(c1.engine.delete(l3id).expect("Delete should work"));

    // Sync c1
    info!("Syncing c1");
    c1.sync().expect("c1 sync to work");
    info!("Checking c1 state after sync");

    verify_missing_login(&c1.engine, l0id);
    verify_missing_login(&c1.engine, l1id);
    verify_login(&c1.engine, &login2);
    verify_missing_login(&c1.engine, l3id);

    info!("Update {} on c0", l3id);
    // 4b
    update_login(&c0.engine, l3id, |l| {
        l.password = "quux".into();
    }).unwrap();

    // Sync c0
    info!("Syncing c0");
    c0.sync().expect("c0 sync to work");

    info!("Checking c0 state after sync");

    verify_missing_login(&c0.engine, l0id);
    verify_missing_login(&c0.engine, l1id);
    verify_login(&c0.engine, &login2_new);
    verify_missing_login(&c0.engine, l3id);

    info!("Delete {} on c1", l2id);
    // 3b
    assert!(c1.engine.delete(l2id).expect("Delete should work"));

    info!("Syncing c1");
    c1.sync().expect("c1 sync to work");

    info!("{} should stay dead", l2id);
    // Ensure we didn't revive it.
    verify_missing_login(&c1.engine, l2id);

    info!("Syncing c0");
    c0.sync().expect("c0 sync to work");
    info!("Should delete {}", l2id);
    verify_missing_login(&c0.engine, l2id);
}

// This is the only #[test] in this function so that we can reuse the TestAccount and TestClients
// without creating instances for every test (note: we can't use lazy_static for this because
// we want `TestAccount::drop` to clean up the account, which isn't run for lazy_statics).
fn test_login_syncing() {
    let test_account = TestAccount::new_random().expect("Failed to initialize test account!");

    let mut c0 = TestClient::new(test_account.clone()).expect("new client 0");
    let mut c1 = TestClient::new(test_account.clone()).expect("new client 1");

    info!("Running test_login_general");
    test_login_general(&mut c0, &mut c1);
    cleanup_clients!(c0, c1);

    info!("Running test_login_deletes");
    test_login_deletes(&mut c0, &mut c1);
    cleanup_clients!(c0, c1);
}

pub fn main() {
    // Hopefully this is enough to be clear in e.g. travis logs.
    println!("### Running sync integration tests ###");
    println!("######################################");
    init_test_logging();
    test_login_syncing();
}





