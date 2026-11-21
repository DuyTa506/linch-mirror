//! Scheduled fires with several capped roots held: a root that is not held
//! fires whatever the number of held ones, and each schedule fires exactly
//! once while locks come and go and schedules change under a waiting firer.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use linch_worker::{
    Cap, Schedule, Timing, create_schedule, delete_schedule, fire_due_schedules, get_schedule,
    pause_schedule,
};
use tokio::time::Instant;

use crate::common::*;

fn every(namespace: &str, id: &str, every: Duration) -> Schedule {
    Schedule {
        namespace: namespace.into(),
        id: id.into(),
        queue: "default".into(),
        session_id: None,
        prompt: "tick".into(),
        timing: Timing::Every(every),
        agent: None,
        principal: None,
    }
}

fn hourly(namespace: &str, id: &str) -> Schedule {
    every(namespace, id, Duration::from_secs(3600))
}

/// A transaction that holds `root`'s job cap lock until it ends.
async fn hold(db: &TestDb, root: &str) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let mut holder = db.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("linch_worker_cap:jobs:{root}"))
        .execute(&mut *holder)
        .await
        .unwrap();
    holder
}

async fn jobs_of(db: &TestDb, namespace: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM linch_worker_jobs WHERE namespace = $1")
        .bind(namespace)
        .fetch_one(&db.pool)
        .await
        .unwrap()
}

/// `held` capped roots, each with one due schedule and its lock held by an
/// open transaction, and one root without a cap whose schedule is due too.
/// Up to `calls` calls; the call at which the free root fired, and the
/// wall time of every call.
async fn free_root_behind_held_roots(
    name: &str,
    held: usize,
    calls: u32,
) -> (Option<u32>, Vec<Duration>) {
    let db = TestDb::new(name).await.expect("LINCH_TEST_PG_DSN is set");
    let pid = std::process::id();
    let roots: Vec<String> = (0..held).map(|i| format!("held{i}x{pid}{name}")).collect();
    let free = format!("free{pid}{name}");
    let caps: Vec<Cap> = (roots.iter())
        .map(|root| Cap {
            root: root.clone(),
            max: 1000,
        })
        .collect();
    for root in &roots {
        create_schedule(&db.pool, &hourly(&format!("{root}/a"), "h"), None)
            .await
            .unwrap();
    }
    create_schedule(&db.pool, &hourly(&format!("{free}/a"), "f"), None)
        .await
        .unwrap();
    db.sql("UPDATE linch_worker_schedules SET next_fire = now() - interval '1 second'")
        .await;
    let mut holders = Vec::new();
    for root in &roots {
        holders.push(hold(&db, root).await);
    }
    let (mut fired_at, mut took) = (None, Vec::new());
    for call in 1..=calls {
        let since = Instant::now();
        fire_due_schedules(&db.pool, &caps).await.unwrap();
        took.push(since.elapsed());
        if jobs_of(&db, &format!("{free}/a")).await > 0 {
            fired_at = Some(call);
            break;
        }
    }
    for holder in holders {
        holder.rollback().await.unwrap();
    }
    db.finish().await;
    (fired_at, took)
}

/// With two held roots the free root fires in the first call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_free_root_fires_while_two_roots_are_held() {
    if std::env::var("LINCH_TEST_PG_DSN").is_err() {
        eprintln!("skipping: LINCH_TEST_PG_DSN is not set");
        return;
    }
    let (fired_at, took) = free_root_behind_held_roots("two", 2, 10).await;
    assert_eq!(fired_at, Some(1), "calls took {took:?}");
}

/// With three or more roots held, a root that has no cap and nothing to do
/// with them fires in every call, and no call takes longer than a lock wait
/// and some.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_free_root_fires_while_many_roots_are_held() {
    if std::env::var("LINCH_TEST_PG_DSN").is_err() {
        eprintln!("skipping: LINCH_TEST_PG_DSN is not set");
        return;
    }
    let (fired_at, took) = free_root_behind_held_roots("many", 6, 10).await;
    assert!(
        took.iter().all(|call| *call < Duration::from_millis(900)),
        "a call took long: {took:?}"
    );
    assert_eq!(
        fired_at,
        Some(1),
        "10 calls never fired the free root's due schedule while three other roots were held \
         (calls took {took:?})"
    );
}

/// Exactly once: five firers loop while three roots' locks are held
/// for 150 to 400 ms at a time (across the fire's 250 ms wait) and a
/// fourth root stays free; 30 schedules a root. Each schedule gets one job
/// and one fire, none is skipped or left due once the locks are gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn firers_racing_locks_held_across_the_wait_fire_each_schedule_once() {
    let Some(db) = TestDb::new("once").await else {
        return;
    };
    let pid = std::process::id();
    let roots: Vec<String> = (0..4).map(|i| format!("once{i}x{pid}")).collect();
    let caps: Vec<Cap> = (roots.iter())
        .map(|root| Cap {
            root: root.clone(),
            max: 1000,
        })
        .collect();
    for root in &roots {
        for i in 0..30 {
            let schedule = hourly(&format!("{root}/a"), &format!("s{i}"));
            create_schedule(&db.pool, &schedule, None).await.unwrap();
        }
    }
    db.sql("UPDATE linch_worker_schedules SET next_fire = now() - interval '1 second'")
        .await;
    let stop = Arc::new(AtomicBool::new(false));
    let flappers: Vec<_> = (0..3usize)
        .map(|i| {
            let (pool, root, stop) = (db.pool.clone(), roots[i].clone(), Arc::clone(&stop));
            tokio::spawn(async move {
                let mut turn = i as u64;
                while !stop.load(Ordering::Relaxed) {
                    let mut holder = pool.begin().await.unwrap();
                    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
                        .bind(format!("linch_worker_cap:jobs:{root}"))
                        .execute(&mut *holder)
                        .await
                        .unwrap();
                    turn += 1;
                    tokio::time::sleep(Duration::from_millis(150 + (turn * 97) % 250)).await;
                    holder.rollback().await.unwrap();
                    tokio::time::sleep(Duration::from_millis(5 + (turn * 31) % 60)).await;
                }
            })
        })
        .collect();
    let firers: Vec<_> = (0..5)
        .map(|_| {
            let (pool, caps) = (db.pool.clone(), caps.clone());
            tokio::spawn(async move {
                let until = Instant::now() + Duration::from_secs(6);
                let mut total = 0;
                while Instant::now() < until {
                    total += fire_due_schedules(&pool, &caps).await.unwrap();
                }
                total
            })
        })
        .collect();
    let mut total = 0;
    for firer in firers {
        total += firer.await.unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    for flapper in flappers {
        flapper.await.unwrap();
    }
    let during = total;
    // The locks are gone: what was left due fires now.
    for _ in 0..6 {
        total += fire_due_schedules(&db.pool, &caps).await.unwrap();
    }
    let per_schedule: Vec<(i64, i64, i64, bool)> = sqlx::query_as(
        "SELECT count(j.id), s.fires, s.skips, s.next_fire > now() + interval '50 minutes' \
         FROM linch_worker_schedules AS s \
         LEFT JOIN linch_worker_jobs AS j \
           ON j.schedule_id = s.id AND j.namespace = s.namespace \
         GROUP BY s.namespace, s.id, s.fires, s.skips, s.next_fire",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    db.finish().await;
    eprintln!("fired {during} while the locks came and went, {total} in all");
    assert_eq!(
        per_schedule,
        vec![(1, 1, 0, true); 120],
        "one job and one fire per schedule"
    );
    assert_eq!(total, 120, "the calls reported each fire once");
}

/// A schedule paused, and another deleted, while a firer waits for their
/// held root: neither fires afterwards, and no call fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_schedule_paused_or_deleted_while_its_root_is_held_does_not_fire() {
    let Some(db) = TestDb::new("pause").await else {
        return;
    };
    let root = format!("pause{}", std::process::id());
    let namespace = format!("{root}/a");
    let caps = [Cap {
        root: root.clone(),
        max: 1000,
    }];
    for id in ["paused", "deleted", "kept"] {
        create_schedule(&db.pool, &hourly(&namespace, id), None)
            .await
            .unwrap();
    }
    db.sql("UPDATE linch_worker_schedules SET next_fire = now() - interval '1 second'")
        .await;
    let holder = hold(&db, &root).await;
    let firer = {
        let (pool, caps) = (db.pool.clone(), caps.clone());
        tokio::spawn(async move {
            let mut fired = 0;
            for _ in 0..12 {
                fired += fire_due_schedules(&pool, &caps).await.unwrap();
            }
            fired
        })
    };
    // The firer is inside its first lock wait, the rows locked by its select.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (pause, delete) = tokio::join!(
        pause_schedule(&db.pool, &namespace, "paused"),
        delete_schedule(&db.pool, &namespace, "deleted")
    );
    assert!(pause.unwrap().unwrap().paused);
    assert!(delete.unwrap());
    tokio::time::sleep(Duration::from_millis(600)).await;
    holder.rollback().await.unwrap();
    let fired = firer.await.unwrap();
    let jobs: Vec<String> = sqlx::query_scalar("SELECT schedule_id FROM linch_worker_jobs")
        .fetch_all(&db.pool)
        .await
        .unwrap();
    let paused = get_schedule(&db.pool, &namespace, "paused").await.unwrap();
    db.finish().await;
    assert_eq!(
        jobs,
        vec!["kept".to_owned()],
        "only the schedule left alone fired"
    );
    assert_eq!(fired, 1);
    assert_eq!(paused.unwrap().fires, 0);
}

/// A one-second schedule whose root is held for 3.5 s fires once when the
/// lock goes, not once per missed tick, and stays on its grid.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_schedule_held_past_several_ticks_fires_once() {
    let Some(db) = TestDb::new("ticks").await else {
        return;
    };
    let root = format!("ticks{}", std::process::id());
    let namespace = format!("{root}/a");
    let caps = [Cap {
        root: root.clone(),
        max: 1000,
    }];
    create_schedule(
        &db.pool,
        &every(&namespace, "tick", Duration::from_secs(1)),
        None,
    )
    .await
    .unwrap();
    let holder = hold(&db, &root).await;
    let since = Instant::now();
    let mut early = 0;
    while since.elapsed() < Duration::from_millis(3500) {
        early += fire_due_schedules(&db.pool, &caps).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    holder.rollback().await.unwrap();
    let mut late = 0;
    for _ in 0..5 {
        late += fire_due_schedules(&db.pool, &caps).await.unwrap();
    }
    let jobs = jobs_of(&db, &namespace).await;
    let info = get_schedule(&db.pool, &namespace, "tick")
        .await
        .unwrap()
        .unwrap();
    let ahead: bool = sqlx::query_scalar("SELECT $1 > now()")
        .bind(info.next_fire)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    db.finish().await;
    assert_eq!(
        (early, late, jobs),
        (0, 1, 1),
        "one fire, after the lock went"
    );
    assert_eq!(info.fires, 1);
    assert!(
        info.skips >= 2,
        "the missed ticks count as skips: {}",
        info.skips
    );
    assert!(ahead, "the next fire is still to come");
}
