//! The update manager against a scripted server, on a paused clock.
//!
//! The scenario tests drive one rule each. The model checker
//! (`kill_at_every_point_loses_no_sequenced_update`) runs 200 random
//! scripts; for each, it kills and restarts the manager at every point of
//! the run in turn (each step, and before admission, after admission and
//! after the commit of each batch) and checks that every sequenced update
//! of the server's log reached admission at least once.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use bytes::Bytes;
use grammers_tl_types::{self as tl, Deserializable, Serializable};
use tgin_mtproto::decode::{BoxKind, PeerId};
use tgin_mtproto::tl_json::from_json;
use tgin_mtproto::updates::{GAP_HOLD, MAX_CHANNEL_FETCHES};
use tgin_mtproto::{Batch, Fetch, PeerKind, StateDelta, StoredState, UpdateManager, UpdateState};
use tokio::time::{Duration, Instant, advance};

// ---------------------------------------------------------------- TL bytes

const VECTOR: u32 = 0x1cb5_c415;
const UPDATES: u32 = 0x74ae_4240;
const UPDATE_SHORT: u32 = 0x78d4_dec1;
const UPDATES_TOO_LONG: u32 = 0xe317_af7e;
const UPDATE_NEW_MESSAGE: u32 = 0x1f2b_0afd;
const UPDATE_NEW_CHANNEL_MESSAGE: u32 = 0x62ba_04d9;
const UPDATE_NEW_ENCRYPTED_MESSAGE: u32 = 0x12bc_bd9a;
const UPDATE_CHANNEL_TOO_LONG: u32 = 0x108d_941f;
const UPDATE_DELETE_MESSAGES: u32 = 0xa20d_b0e5;
const STATE: u32 = 0xa56c_2a3e;
const DIFFERENCE_EMPTY: u32 = 0x5d75_a138;
const DIFFERENCE: u32 = 0x00f4_9ca0;
const DIFFERENCE_SLICE: u32 = 0xa8fb_1981;
const CHANNEL_DIFFERENCE_EMPTY: u32 = 0x3e11_affb;
const CHANNEL_DIFFERENCE: u32 = 0x2064_674e;

const NONE: &[Vec<u8>] = &[];

/// A TL byte builder.
struct T(Vec<u8>);

impl T {
    fn new(id: u32) -> Self {
        T(id.to_le_bytes().to_vec())
    }
    fn int(mut self, v: i32) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn long(mut self, v: i64) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn raw(mut self, b: &[u8]) -> Self {
        self.0.extend_from_slice(b);
        self
    }
    fn vec<B: AsRef<[u8]>>(mut self, items: &[B]) -> Self {
        self.0.extend_from_slice(&VECTOR.to_le_bytes());
        let n = i32::try_from(items.len()).expect("vector length");
        self.0.extend_from_slice(&n.to_le_bytes());
        for i in items {
            self.0.extend_from_slice(i.as_ref());
        }
        self
    }
    fn done(self) -> Vec<u8> {
        self.0
    }
}

fn tl(json: &str) -> Vec<u8> {
    let mut b = Vec::new();
    if let Err(e) = from_json(json, &mut b) {
        panic!("{e}: {json}");
    }
    b
}

fn message(id: u32, peer: PeerId, text: &str) -> Vec<u8> {
    let peer = match peer {
        PeerId::User(u) => format!(r#"{{"_":"peerUser","user_id":"{u}"}}"#),
        PeerId::Chat(c) => format!(r#"{{"_":"peerChat","chat_id":"{c}"}}"#),
        PeerId::Channel(c) => format!(r#"{{"_":"peerChannel","channel_id":"{c}"}}"#),
    };
    let text = serde_json::to_string(text).expect("text");
    tl(&format!(
        r#"{{"_":"message","id":{id},"peer_id":{peer},"date":1,"message":{text}}}"#
    ))
}

fn encrypted(id: u32) -> Vec<u8> {
    tl(&format!(
        r#"{{"_":"encryptedMessageService","random_id":"{id}","chat_id":1,"date":1,"bytes":"AAAA"}}"#
    ))
}

fn user(id: i64) -> Vec<u8> {
    tl(&format!(
        r#"{{"_":"user","id":"{id}","access_hash":"{}"}}"#,
        id * 7 + 1
    ))
}

fn channel(id: i64) -> Vec<u8> {
    tl(&format!(
        r#"{{"_":"channel","id":"{id}","access_hash":"{}","title":"c","photo":{{"_":"chatPhotoEmpty"}},"date":0}}"#,
        id * 11 + 3
    ))
}

fn short_message(id: u32, user: i64, pts: i32, count: i32) -> Vec<u8> {
    tl(&format!(
        r#"{{"_":"updateShortMessage","id":{id},"user_id":"{user}","message":"e{id}","pts":{pts},"pts_count":{count},"date":1}}"#
    ))
}

fn user_status(user: i64) -> Vec<u8> {
    tl(&format!(
        r#"{{"_":"updateUserStatus","user_id":"{user}","status":{{"_":"userStatusOnline","expires":5}}}}"#
    ))
}

fn new_message(m: &[u8], pts: i32, count: i32) -> Vec<u8> {
    T::new(UPDATE_NEW_MESSAGE).raw(m).int(pts).int(count).done()
}

fn new_channel_message(m: &[u8], pts: i32, count: i32) -> Vec<u8> {
    T::new(UPDATE_NEW_CHANNEL_MESSAGE)
        .raw(m)
        .int(pts)
        .int(count)
        .done()
}

fn new_encrypted(m: &[u8], qts: i32) -> Vec<u8> {
    T::new(UPDATE_NEW_ENCRYPTED_MESSAGE).raw(m).int(qts).done()
}

fn channel_too_long(c: i64, pts: Option<i32>) -> Vec<u8> {
    let t = T::new(UPDATE_CHANNEL_TOO_LONG)
        .int(i32::from(pts.is_some()))
        .long(c);
    match pts {
        Some(p) => t.int(p).done(),
        None => t.done(),
    }
}

fn delete_messages(ids: &[i32], pts: i32, count: i32) -> Vec<u8> {
    let ids: Vec<[u8; 4]> = ids.iter().map(|i| i.to_le_bytes()).collect();
    T::new(UPDATE_DELETE_MESSAGES)
        .vec(&ids)
        .int(pts)
        .int(count)
        .done()
}

fn short(update: &[u8]) -> Vec<u8> {
    T::new(UPDATE_SHORT).raw(update).int(1).done()
}

fn updates(us: &[Vec<u8>], users: &[Vec<u8>], chats: &[Vec<u8>], date: i32, seq: i32) -> Vec<u8> {
    T::new(UPDATES)
        .vec(us)
        .vec(users)
        .vec(chats)
        .int(date)
        .int(seq)
        .done()
}

fn state(pts: i32, qts: i32, date: i32, seq: i32) -> Vec<u8> {
    T::new(STATE)
        .int(pts)
        .int(qts)
        .int(date)
        .int(seq)
        .int(0)
        .done()
}

struct Diff<'a> {
    msgs: &'a [Vec<u8>],
    enc: &'a [Vec<u8>],
    other: &'a [Vec<u8>],
    chats: &'a [Vec<u8>],
    users: &'a [Vec<u8>],
}

const EMPTY_DIFF: Diff<'static> = Diff {
    msgs: NONE,
    enc: NONE,
    other: NONE,
    chats: NONE,
    users: NONE,
};

fn difference(d: &Diff<'_>, st: &[u8], slice: bool) -> Vec<u8> {
    T::new(if slice { DIFFERENCE_SLICE } else { DIFFERENCE })
        .vec(d.msgs)
        .vec(d.enc)
        .vec(d.other)
        .vec(d.chats)
        .vec(d.users)
        .raw(st)
        .done()
}

fn difference_empty(date: i32, seq: i32) -> Vec<u8> {
    T::new(DIFFERENCE_EMPTY).int(date).int(seq).done()
}

fn channel_difference(fin: bool, pts: i32, d: &Diff<'_>) -> Vec<u8> {
    T::new(CHANNEL_DIFFERENCE)
        .int(i32::from(fin))
        .int(pts)
        .vec(d.msgs)
        .vec(d.other)
        .vec(d.chats)
        .vec(d.users)
        .done()
}

fn channel_difference_empty(fin: bool, pts: i32) -> Vec<u8> {
    T::new(CHANNEL_DIFFERENCE_EMPTY)
        .int(i32::from(fin))
        .int(pts)
        .done()
}

fn b(v: Vec<u8>) -> Bytes {
    Bytes::from(v)
}

// ----------------------------------------------------------- scenario help

fn stored(pts: i32, channels: &[(i64, i32)], known: &[i64]) -> StoredState {
    StoredState {
        common: UpdateState {
            pts,
            qts: 5,
            seq: 3,
            date: 1,
        },
        channels: channels.to_vec(),
        known_users: known.to_vec(),
    }
}

/// Commits every batch in order; returns what was to be persisted.
fn commit(m: &mut UpdateManager, batches: &[Batch]) -> StateDelta {
    let mut out = StateDelta::default();
    for batch in batches {
        if let Some(d) = m.committed(batch.id) {
            out.pts = d.pts.or(out.pts);
            out.qts = d.qts.or(out.qts);
            out.seq = d.seq.or(out.seq);
            out.date = d.date.or(out.date);
            out.channels.extend(d.channels);
        }
    }
    out
}

/// A manager past its start-up difference (an empty one).
fn started(st: StoredState) -> UpdateManager {
    let pts = st.common.pts;
    let mut m = UpdateManager::new(st);
    assert_eq!(
        m.next_action(),
        Some(Fetch::GetDifference {
            pts,
            qts: 5,
            date: 1
        })
    );
    let batches = m.feed_difference(b(difference_empty(1, 3)));
    commit(&mut m, &batches);
    assert_eq!(m.next_action(), None);
    m
}

fn all_updates(batches: &[Batch]) -> Vec<Bytes> {
    batches
        .iter()
        .flat_map(|b| b.updates.iter().map(|u| u.bytes().clone()))
        .collect()
}

// --------------------------------------------------------------- scenarios

#[tokio::test(start_paused = true)]
async fn in_order_pushes_delivered_immediately() {
    let mut m = started(stored(10, &[(7, 40)], &[5]));
    let m11 = message(1, PeerId::User(5), "one");
    let u11 = new_message(&m11, 11, 1);
    let push = b(updates(std::slice::from_ref(&u11), &[user(5)], NONE, 2, 4));
    let batches = m.feed_push(push);
    assert_eq!(all_updates(&batches), vec![b(u11)]);
    assert_eq!(
        batches[0].users,
        b(T::new(VECTOR).int(1).raw(&user(5)).done())
    );
    let d = commit(&mut m, &batches);
    assert_eq!((d.pts, d.seq, d.date), (Some(11), Some(4), Some(2)));

    // A short message from a cached user, then a channel post: each at once.
    let s12 = b(short_message(2, 5, 12, 1));
    let batches = m.feed_push(s12.clone());
    assert_eq!(all_updates(&batches), vec![s12]);
    commit(&mut m, &batches);
    let c41 = new_channel_message(&message(3, PeerId::Channel(7), "post"), 41, 1);
    let batches = m.feed_push(b(updates(
        std::slice::from_ref(&c41),
        NONE,
        &[channel(7)],
        3,
        5,
    )));
    assert_eq!(all_updates(&batches), vec![b(c41)]);
    assert_eq!(commit(&mut m, &batches).channels, vec![(7, 41)]);
    assert_eq!(m.next_action(), None, "nothing to fetch");
}

/// A container whose seq is already applied (it arrived after the
/// difference that moved seq past it) still carries box updates: each is
/// offered by its own box's rules. Only its unsequenced updates go.
#[tokio::test(start_paused = true)]
async fn stale_seq_container_routes_box_updates() {
    let mut m = started(stored(10, &[(1, 5), (2, 9)], &[5]));
    let post = new_channel_message(&message(1, PeerId::Channel(1), "p"), 6, 1);
    let applied = new_message(&message(2, PeerId::User(5), "old"), 10, 1);
    let push = b(updates(
        &[
            user_status(5),
            applied,
            post.clone(),
            channel_too_long(2, None),
        ],
        NONE,
        NONE,
        2,
        3, // seq 3: already applied
    ));
    let batches = m.feed_push(push);
    assert_eq!(
        all_updates(&batches),
        vec![b(post)],
        "the channel post only"
    );
    let d = commit(&mut m, &batches);
    assert_eq!(d.channels, vec![(1, 6)]);
    assert_eq!((d.seq, d.date), (None, None), "the seq state is kept");
    assert_eq!(
        m.next_action(),
        Some(Fetch::GetChannelDifference { channel: 2, pts: 9 }),
        "its updateChannelTooLong is a signal still"
    );
}

#[tokio::test(start_paused = true)]
async fn gap_triggers_difference_after_hold() {
    let mut m = started(stored(10, &[], &[5]));
    let msg = |i: u32| message(i, PeerId::User(5), &format!("m{i}"));
    // A reordered pair inside the hold is delivered in order.
    let u12 = new_message(&msg(12), 12, 1);
    let u11 = new_message(&msg(11), 11, 1);
    assert!(
        m.feed_push(b(updates(std::slice::from_ref(&u12), NONE, NONE, 1, 0)))
            .is_empty()
    );
    let batches = m.feed_push(b(updates(std::slice::from_ref(&u11), NONE, NONE, 1, 0)));
    assert_eq!(all_updates(&batches), vec![b(u11), b(u12)]);
    commit(&mut m, &batches);

    // A gap nothing fills: no fetch until the hold is over.
    let u14 = new_message(&msg(14), 14, 1);
    assert!(m.feed_push(b(updates(&[u14], NONE, NONE, 1, 0))).is_empty());
    assert_eq!(m.next_action(), None);
    advance(GAP_HOLD - Duration::from_millis(1)).await;
    assert_eq!(m.next_action(), None, "still inside the hold");
    advance(Duration::from_millis(1)).await;
    assert_eq!(
        m.next_action(),
        Some(Fetch::GetDifference {
            pts: 12,
            qts: 5,
            date: 1
        })
    );
    let msgs = [msg(13), msg(14)];
    let batches = m.feed_difference(b(difference(
        &Diff {
            msgs: &msgs,
            ..EMPTY_DIFF
        },
        &state(14, 5, 2, 3),
        false,
    )));
    // The held 14 is covered by the difference and not repeated.
    assert_eq!(
        all_updates(&batches),
        vec![
            b(new_message(&msgs[0], 14, 0)),
            b(new_message(&msgs[1], 14, 0))
        ]
    );
    assert_eq!(commit(&mut m, &batches).pts, Some(14));
    assert_eq!(m.next_action(), None);
}

#[tokio::test(start_paused = true)]
async fn difference_slices_until_final() {
    let mut m = UpdateManager::new(stored(10, &[], &[5]));
    assert!(matches!(
        m.next_action(),
        Some(Fetch::GetDifference { pts: 10, .. })
    ));
    let msg = |i: u32| message(i, PeerId::User(5), &format!("m{i}"));
    let first = [msg(11), msg(12)];
    let batches = m.feed_difference(b(difference(
        &Diff {
            msgs: &first,
            ..EMPTY_DIFF
        },
        &state(12, 6, 2, 4),
        true,
    )));
    assert_eq!(all_updates(&batches).len(), 2);
    // Every update of the slice carries the slice's final pts, and the
    // slice is one commit.
    assert!(
        batches
            .iter()
            .flat_map(|b| &b.updates)
            .all(|u| u.facts().pts == Some(12))
    );
    assert_eq!(batches.len(), 1);
    let d = commit(&mut m, &batches);
    assert_eq!((d.pts, d.qts, d.seq), (Some(12), Some(6), Some(4)));

    // A live push while the box catches up waits for the final part.
    let live = new_message(&msg(14), 14, 1);
    assert!(
        m.feed_push(b(updates(std::slice::from_ref(&live), NONE, NONE, 3, 0)))
            .is_empty()
    );
    assert_eq!(
        m.next_action(),
        Some(Fetch::GetDifference {
            pts: 12,
            qts: 6,
            date: 2
        })
    );
    assert_eq!(m.next_action(), None, "one difference at a time");
    let last = [msg(13)];
    let batches = m.feed_difference(b(difference(
        &Diff {
            msgs: &last,
            ..EMPTY_DIFF
        },
        &state(13, 6, 3, 5),
        false,
    )));
    assert_eq!(
        all_updates(&batches),
        vec![b(new_message(&last[0], 13, 0)), b(live)]
    );
    assert_eq!(commit(&mut m, &batches).pts, Some(14));
    assert_eq!(m.next_action(), None, "final: no more slices");
}

#[tokio::test(start_paused = true)]
async fn live_push_not_blocked_by_1000_channel_catchup() {
    let behind: Vec<(i64, i32)> = (1..=1000).map(|c| (c, 1)).collect();
    let mut channels = behind.clone();
    channels.push((5000, 1));
    let mut m = UpdateManager::new(stored(10, &channels, &[5]));
    assert!(matches!(m.next_action(), Some(Fetch::GetDifference { .. })));
    let other: Vec<Vec<u8>> = behind
        .iter()
        .map(|&(c, _)| channel_too_long(c, Some(50)))
        .collect();
    let batches = m.feed_difference(b(difference(
        &Diff {
            other: &other,
            ..EMPTY_DIFF
        },
        &state(10, 5, 1, 3),
        false,
    )));
    assert!(
        all_updates(&batches).is_empty(),
        "signals are not delivered"
    );
    commit(&mut m, &batches);

    let mut asked = Vec::new();
    while let Some(f) = m.next_action() {
        asked.push(f);
    }
    assert_eq!(asked.len(), MAX_CHANNEL_FETCHES);

    // Live pushes, common and in a channel that is not behind, are
    // delivered at once while 1 000 channels wait.
    let live = new_message(&message(1, PeerId::User(5), "live"), 11, 1);
    let batches = m.feed_push(b(updates(std::slice::from_ref(&live), NONE, NONE, 2, 0)));
    assert_eq!(all_updates(&batches), vec![b(live)]);
    commit(&mut m, &batches);
    let post = new_channel_message(&message(2, PeerId::Channel(5000), "p"), 2, 1);
    let batches = m.feed_push(b(updates(std::slice::from_ref(&post), NONE, NONE, 2, 0)));
    assert_eq!(all_updates(&batches), vec![b(post)]);
    commit(&mut m, &batches);

    // Every channel then catches up exactly once.
    let mut fetched = BTreeMap::new();
    let mut queue: VecDeque<Fetch> = asked.into();
    while let Some(f) = queue.pop_front() {
        let Fetch::GetChannelDifference { channel, pts } = f else {
            panic!("unexpected {f:?}");
        };
        assert_eq!(pts, 1);
        *fetched.entry(channel).or_insert(0) += 1;
        let batches = m.feed_channel_difference(channel, b(channel_difference_empty(true, 50)));
        assert_eq!(commit(&mut m, &batches).channels, vec![(channel, 50)]);
        while let Some(f) = m.next_action() {
            queue.push_back(f);
        }
    }
    assert_eq!(fetched.len(), 1000);
    assert!(fetched.values().all(|&n| n == 1));
}

#[tokio::test(start_paused = true)]
async fn channel_gap_fetches_only_that_channel() {
    let mut m = started(stored(10, &[(1, 5), (2, 7)], &[]));
    let post = |id: u32, c: i64| message(id, PeerId::Channel(c), &format!("p{id}"));
    let a7 = new_channel_message(&post(7, 1), 7, 1);
    assert!(m.feed_push(b(updates(&[a7], NONE, NONE, 2, 0))).is_empty());
    advance(GAP_HOLD).await;
    assert_eq!(
        m.next_action(),
        Some(Fetch::GetChannelDifference { channel: 1, pts: 5 })
    );
    assert_eq!(m.next_action(), None, "no other channel, no common box");

    // Channel 2 is not held back meanwhile.
    let b8 = new_channel_message(&post(8, 2), 8, 1);
    let batches = m.feed_push(b(updates(std::slice::from_ref(&b8), NONE, NONE, 2, 0)));
    assert_eq!(all_updates(&batches), vec![b(b8)]);
    commit(&mut m, &batches);

    let msgs = [post(6, 1), post(7, 1)];
    let batches = m.feed_channel_difference(
        1,
        b(channel_difference(
            true,
            7,
            &Diff {
                msgs: &msgs,
                chats: &[channel(1)],
                ..EMPTY_DIFF
            },
        )),
    );
    // Composed `updateNewChannelMessage`s carry the channel's pts; the held
    // push is covered.
    assert_eq!(
        all_updates(&batches),
        vec![
            b(new_channel_message(&msgs[0], 7, 0)),
            b(new_channel_message(&msgs[1], 7, 0))
        ]
    );
    assert_eq!(commit(&mut m, &batches).channels, vec![(1, 7)]);
    assert_eq!(m.next_action(), None);
}

#[tokio::test(start_paused = true)]
async fn channel_too_long_fetches_once() {
    let mut m = started(stored(10, &[(1, 5), (2, 9)], &[]));
    for _ in 0..2 {
        assert!(
            m.feed_push(b(short(&channel_too_long(1, Some(9)))))
                .is_empty()
        );
    }
    assert_eq!(
        m.next_action(),
        Some(Fetch::GetChannelDifference { channel: 1, pts: 5 })
    );
    assert_eq!(m.next_action(), None, "pending signals join one fetch");
    let batches = m.feed_channel_difference(1, b(channel_difference_empty(true, 9)));
    assert_eq!(commit(&mut m, &batches).channels, vec![(1, 9)]);
    assert_eq!(m.next_action(), None, "final: fetched once");

    // Without a pts, the signal still fetches.
    assert!(m.feed_push(b(short(&channel_too_long(2, None)))).is_empty());
    assert_eq!(
        m.next_action(),
        Some(Fetch::GetChannelDifference { channel: 2, pts: 9 })
    );
    // An unknown channel has nothing to catch up from: its pts becomes its
    // state, persisted, and nothing is fetched.
    let batches = m.feed_push(b(short(&channel_too_long(3, Some(70)))));
    assert_eq!(commit(&mut m, &batches).channels, vec![(3, 70)]);
    assert_eq!(m.next_action(), None);
}

/// An `updateChannelTooLong` fed while that channel's call is in flight
/// may postdate the call's answer (pushes and answers travel apart): one
/// more fetch follows, even after a `final` answer, however many came.
#[tokio::test(start_paused = true)]
async fn channel_too_long_mid_fetch_fetches_once_more() {
    let mut m = started(stored(10, &[(1, 5)], &[]));
    assert!(m.feed_push(b(short(&channel_too_long(1, None)))).is_empty());
    assert_eq!(
        m.next_action(),
        Some(Fetch::GetChannelDifference { channel: 1, pts: 5 })
    );
    for _ in 0..2 {
        assert!(m.feed_push(b(short(&channel_too_long(1, None)))).is_empty());
        assert_eq!(m.next_action(), None, "no parallel fetch");
    }
    let batches = m.feed_channel_difference(1, b(channel_difference_empty(true, 7)));
    commit(&mut m, &batches);
    assert_eq!(
        m.next_action(),
        Some(Fetch::GetChannelDifference { channel: 1, pts: 7 }),
        "one more after a final answer"
    );
    let batches = m.feed_channel_difference(1, b(channel_difference_empty(true, 9)));
    assert_eq!(commit(&mut m, &batches).channels, vec![(1, 9)]);
    assert_eq!(m.next_action(), None, "exactly one more");
}

#[tokio::test(start_paused = true)]
async fn unknown_peer_short_update_gets_entities() {
    let mut m = started(stored(10, &[], &[]));
    // pts follows on, but user 42 has no cached access hash: fetch the
    // difference that covers it, at once.
    assert!(m.feed_push(b(short_message(1, 42, 11, 1))).is_empty());
    assert_eq!(
        m.next_action(),
        Some(Fetch::GetDifference {
            pts: 10,
            qts: 5,
            date: 1
        })
    );
    let msg = message(1, PeerId::User(42), "e1");
    let users = [user(42)];
    let batches = m.feed_difference(b(difference(
        &Diff {
            msgs: std::slice::from_ref(&msg),
            users: &users,
            ..EMPTY_DIFF
        },
        &state(11, 5, 2, 3),
        false,
    )));
    assert_eq!(all_updates(&batches), vec![b(new_message(&msg, 11, 0))]);
    assert_eq!(
        batches[0].users,
        b(T::new(VECTOR).int(1).raw(&users[0]).done())
    );
    assert!(batches[0].peers.iter().any(|p| p.id == 42
        && matches!(p.kind, PeerKind::User { .. })
        && p.access_hash == Some(42 * 7 + 1)));
    commit(&mut m, &batches);
    // Now cached: the next short message is delivered at once.
    let s = b(short_message(2, 42, 12, 1));
    assert_eq!(all_updates(&m.feed_push(s.clone())), vec![s]);
    assert_eq!(m.next_action(), None);
}

/// A fresh account's first state is refused: nothing was ever committed,
/// so the box resyncs with getState, never a difference from pts 0
/// (PERSISTENT_TIMESTAMP_EMPTY). Also when the refusal lands while a call
/// is in flight.
#[tokio::test(start_paused = true)]
async fn rollback_on_fresh_account_resyncs_with_get_state() {
    let fresh = || UpdateManager::new(stored(0, &[], &[5]));
    let mut m = fresh();
    assert_eq!(m.next_action(), Some(Fetch::GetState));
    let u = new_message(&message(1, PeerId::User(5), "a"), 21, 1);
    assert!(m.feed_push(b(updates(&[u], NONE, NONE, 2, 0))).is_empty());
    let batches = m.feed_state(b(state(20, 6, 2, 4)));
    assert_eq!(all_updates(&batches).len(), 1);
    assert_eq!(m.refused(batches[0].id), None);
    assert_eq!(m.next_action(), Some(Fetch::GetState));

    // Refused while a difference is in flight: its answer is stale.
    let mut m = fresh();
    assert_eq!(m.next_action(), Some(Fetch::GetState));
    let batches = m.feed_state(b(state(20, 6, 2, 4)));
    m.feed_push(too_long());
    assert!(matches!(
        m.next_action(),
        Some(Fetch::GetDifference { pts: 20, .. })
    ));
    m.refused(batches[0].id);
    assert!(m.feed_difference(b(difference_empty(2, 4))).is_empty());
    assert_eq!(m.next_action(), Some(Fetch::GetState));
}

/// A refusal while a getState resync is pending (after an undecodable
/// difference) catches up from the committed state instead: getState would
/// take the server state whole, past the refused updates.
#[tokio::test(start_paused = true)]
async fn rollback_while_get_state_pending_fetches_difference() {
    let mut m = started(stored(10, &[], &[5]));
    let u11 = new_message(&message(1, PeerId::User(5), "a"), 11, 1);
    let batches = m.feed_push(b(updates(&[u11], NONE, NONE, 2, 0)));
    m.feed_push(too_long());
    assert!(matches!(
        m.next_action(),
        Some(Fetch::GetDifference { pts: 11, .. })
    ));
    let mut bad = difference(&EMPTY_DIFF, &state(12, 5, 2, 3), false);
    bad[0] ^= 0xff; // an unknown constructor
    assert!(m.feed_difference(b(bad)).is_empty());
    m.refused(batches[0].id);
    assert_eq!(
        m.next_action(),
        Some(Fetch::GetDifference {
            pts: 10,
            qts: 5,
            date: 1
        })
    );
}

#[tokio::test(start_paused = true)]
async fn unsequenced_drop_counted_when_full() {
    let mut m = started(stored(10, &[], &[5]));
    let msg = message(1, PeerId::User(5), "x");
    let u11 = new_message(&msg, 11, 1);
    let batches = m.feed_push(b(updates(&[user_status(5), u11.clone()], NONE, NONE, 2, 0)));
    assert_eq!(batches.len(), 1);
    // Admission is full: the batch is refused.
    assert_eq!(m.refused(batches[0].id), None, "nothing to persist");
    assert_eq!(m.dropped().full, 1, "the unsequenced update is counted");
    // The sequenced one comes back through the difference.
    assert_eq!(
        m.next_action(),
        Some(Fetch::GetDifference {
            pts: 10,
            qts: 5,
            date: 1
        })
    );
    let batches = m.feed_difference(b(difference(
        &Diff {
            msgs: std::slice::from_ref(&msg),
            ..EMPTY_DIFF
        },
        &state(11, 5, 2, 3),
        false,
    )));
    assert_eq!(all_updates(&batches), vec![b(new_message(&msg, 11, 0))]);
    assert_eq!(commit(&mut m, &batches).pts, Some(11));
}

#[tokio::test(start_paused = true)]
async fn undecodable_is_counted_skipped_and_never_refetched() {
    let mut m = started(stored(10, &[], &[5]));
    // An unknown flag bit (bit 31 of updateShortMessage's flags).
    let mut bad = short_message(1, 5, 11, 1);
    bad[7] |= 0x80;
    assert!(m.feed_push(b(bad)).is_empty());
    assert_eq!(m.dropped().undecodable, 1);

    // A difference with an unknown flag bit is skipped past with a
    // getState resync, never asked for again.
    m.feed_push(b(T::new(UPDATES_TOO_LONG).done()));
    assert!(matches!(
        m.next_action(),
        Some(Fetch::GetDifference { pts: 10, .. })
    ));
    let mut d = difference(
        &Diff {
            msgs: &[message(1, PeerId::User(5), "x")],
            ..EMPTY_DIFF
        },
        &state(11, 5, 2, 3),
        false,
    );
    // id(4) vector(4) count(4) message id(4), then the message's flags.
    d[19] |= 0x80;
    assert!(m.feed_difference(b(d)).is_empty());
    assert_eq!(m.dropped().undecodable, 2);
    assert_eq!(m.next_action(), Some(Fetch::GetState));
    let batches = m.feed_state(b(state(30, 6, 9, 8)));
    let d = commit(&mut m, &batches);
    assert_eq!(
        (d.pts, d.qts, d.date, d.seq),
        (Some(30), Some(6), Some(9), Some(8))
    );
    assert_eq!(m.next_action(), None);
}

/// A failed common fetch is asked again after 1 s, doubling to 60 s, and
/// `deadline` says when; an answer resets it. A permanent failure of a
/// difference resyncs with getState.
#[tokio::test(start_paused = true)]
async fn failed_fetch_backs_off_and_escalates() {
    let mut m = started(stored(10, &[], &[5]));
    m.feed_push(too_long());
    let mut f = m.next_action().expect("a difference");
    for secs in [1, 2, 4, 8, 16, 32, 60, 60] {
        m.fetch_failed(f);
        assert_eq!(m.next_action(), None, "not at once after {secs} s");
        let wait = Duration::from_secs(secs);
        assert_eq!(m.deadline(), Instant::now() + wait);
        advance(wait - Duration::from_millis(1)).await;
        assert_eq!(m.next_action(), None);
        advance(Duration::from_millis(1)).await;
        f = m.next_action().expect("asked again");
        assert!(matches!(f, Fetch::GetDifference { pts: 10, .. }));
    }
    // An answer resets the backoff.
    let batches = m.feed_difference(b(difference_empty(1, 3)));
    commit(&mut m, &batches);
    m.feed_push(too_long());
    let f = m.next_action().expect("at once");
    m.fetch_failed(f);
    assert_eq!(m.deadline(), Instant::now() + Duration::from_secs(1));

    // PERSISTENT_TIMESTAMP_INVALID and the like: resync, at once.
    advance(Duration::from_secs(1)).await;
    let f = m.next_action().expect("asked again");
    assert!(m.fetch_failed_permanently(f).is_empty());
    assert_eq!(m.next_action(), Some(Fetch::GetState));
}

/// A failed channel fetch backs off per channel, its updates waiting, while
/// other boxes go on; a permanent failure drops the channel's state so its
/// next update starts it again, never fetching from the failed state.
#[tokio::test(start_paused = true)]
async fn failed_channel_fetch_backs_off_permanent_drops_state() {
    let mut m = started(stored(10, &[(1, 5), (2, 5)], &[5]));
    m.feed_push(b(short(&channel_too_long(1, None))));
    let f = m.next_action().expect("channel 1");
    assert_eq!(f, Fetch::GetChannelDifference { channel: 1, pts: 5 });
    m.fetch_failed(f);
    assert_eq!(m.next_action(), None);
    assert_eq!(m.deadline(), Instant::now() + Duration::from_secs(1));
    let held = new_channel_message(&message(1, PeerId::Channel(1), "h"), 9, 1);
    assert!(
        m.feed_push(b(updates(std::slice::from_ref(&held), NONE, NONE, 2, 0)))
            .is_empty(),
        "channel 1 is catching up: its update waits"
    );
    let other = new_channel_message(&message(2, PeerId::Channel(2), "o"), 6, 1);
    let batches = m.feed_push(b(updates(std::slice::from_ref(&other), NONE, NONE, 2, 0)));
    assert_eq!(all_updates(&batches), vec![b(other)]);
    commit(&mut m, &batches);
    advance(Duration::from_secs(1)).await;
    let f = m.next_action().expect("asked again");
    m.fetch_failed(f);
    assert_eq!(m.deadline(), Instant::now() + Duration::from_secs(2));
    advance(Duration::from_secs(2)).await;
    let f = m.next_action().expect("asked again");
    // CHANNEL_INVALID: the held update starts the channel again.
    let batches = m.fetch_failed_permanently(f);
    assert_eq!(all_updates(&batches), vec![b(held)]);
    assert_eq!(commit(&mut m, &batches).channels, vec![(1, 8), (1, 9)]);
    assert_eq!(m.next_action(), None);
}

#[tokio::test(start_paused = true)]
async fn affected_pts_advance_the_box() {
    let mut m = started(stored(10, &[(1, 5)], &[5]));
    let batches = m.feed_affected(BoxKind::Common, 12, 2);
    assert_eq!(commit(&mut m, &batches).pts, Some(12));
    let batches = m.feed_affected(BoxKind::Channel(1), 6, 1);
    assert_eq!(commit(&mut m, &batches).channels, vec![(1, 6)]);
    // The next push follows on: no gap, no fetch.
    let u = new_message(&message(1, PeerId::User(5), "a"), 13, 1);
    assert_eq!(
        all_updates(&m.feed_push(b(updates(std::slice::from_ref(&u), NONE, NONE, 2, 0)))),
        vec![b(u)]
    );
    advance(GAP_HOLD * 2).await;
    assert_eq!(m.next_action(), None);
}

fn too_long() -> Bytes {
    b(T::new(UPDATES_TOO_LONG).done())
}

/// After a reconnect the sender pushes two synthetic `updatesTooLong` (the
/// dropped connection, then `new_session_created`): while the difference
/// is only pending, they run one chain.
#[tokio::test(start_paused = true)]
async fn back_to_back_updates_too_long_run_one_chain() {
    let mut m = started(stored(10, &[], &[5]));
    assert!(m.feed_push(too_long()).is_empty());
    assert!(m.feed_push(too_long()).is_empty(), "pending: joins it");
    assert_eq!(
        m.next_action(),
        Some(Fetch::GetDifference {
            pts: 10,
            qts: 5,
            date: 1
        })
    );
    assert_eq!(m.next_action(), None, "one difference");

    // The chain: a slice; a signal between its calls joins the next one.
    let first = [message(11, PeerId::User(5), "m11")];
    let batches = m.feed_difference(b(difference(
        &Diff {
            msgs: &first,
            ..EMPTY_DIFF
        },
        &state(11, 5, 2, 4),
        true,
    )));
    commit(&mut m, &batches);
    assert!(m.feed_push(too_long()).is_empty());
    assert_eq!(
        m.next_action(),
        Some(Fetch::GetDifference {
            pts: 11,
            qts: 5,
            date: 2
        })
    );
    assert_eq!(m.next_action(), None);
    let batches = m.feed_difference(b(difference_empty(2, 4)));
    commit(&mut m, &batches);
    assert_eq!(m.next_action(), None, "one chain");
    advance(GAP_HOLD * 4).await;
    assert_eq!(m.next_action(), None);
}

/// A signal while a call is in flight may postdate that call, so it is not
/// dropped: exactly one more difference follows the chain, however many
/// signals came.
#[tokio::test(start_paused = true)]
async fn updates_too_long_mid_chain_runs_one_more_difference() {
    let mut m = started(stored(10, &[], &[5]));
    assert!(m.feed_push(too_long()).is_empty());
    assert!(matches!(
        m.next_action(),
        Some(Fetch::GetDifference { pts: 10, .. })
    ));
    for _ in 0..2 {
        assert!(m.feed_push(too_long()).is_empty());
        assert_eq!(m.next_action(), None, "no parallel chain");
    }
    let msgs = [message(11, PeerId::User(5), "m11")];
    let batches = m.feed_difference(b(difference(
        &Diff {
            msgs: &msgs,
            ..EMPTY_DIFF
        },
        &state(11, 5, 2, 4),
        false,
    )));
    commit(&mut m, &batches);
    assert_eq!(
        m.next_action(),
        Some(Fetch::GetDifference {
            pts: 11,
            qts: 5,
            date: 2
        }),
        "one more after the chain"
    );
    assert_eq!(m.next_action(), None);
    let batches = m.feed_difference(b(difference_empty(2, 4)));
    commit(&mut m, &batches);
    assert_eq!(m.next_action(), None, "exactly one more");
    advance(GAP_HOLD * 4).await;
    assert_eq!(m.next_action(), None);
}

#[tokio::test(start_paused = true)]
async fn difference_message_composed_bytes_equal_typed() {
    let mut m = UpdateManager::new(stored(10, &[(9, 3)], &[5]));
    assert!(matches!(m.next_action(), Some(Fetch::GetDifference { .. })));
    let msgs = [
        message(1, PeerId::User(5), "plain"),
        message(2, PeerId::Chat(6), "ünïcødé ✓ 日本語 🎉"),
    ];
    let enc = [encrypted(3)];
    let batches = m.feed_difference(b(difference(
        &Diff {
            msgs: &msgs,
            enc: &enc,
            ..EMPTY_DIFF
        },
        &state(20, 8, 2, 4),
        false,
    )));
    let got = all_updates(&batches);
    let typed = |m: &[u8]| tl::enums::Message::from_bytes(m).expect("typed message");
    let mut want: Vec<Vec<u8>> = msgs
        .iter()
        .map(|m| {
            tl::enums::Update::NewMessage(tl::types::UpdateNewMessage {
                message: typed(m),
                pts: 20,
                pts_count: 0,
            })
            .to_bytes()
        })
        .collect();
    want.push(
        tl::enums::Update::NewEncryptedMessage(tl::types::UpdateNewEncryptedMessage {
            message: tl::enums::EncryptedMessage::from_bytes(&enc[0]).expect("typed"),
            qts: 8,
        })
        .to_bytes(),
    );
    assert_eq!(got.iter().map(|g| g.to_vec()).collect::<Vec<_>>(), want);
    commit(&mut m, &batches);

    let post = message(4, PeerId::Channel(9), "κανάλι");
    m.feed_push(b(short(&channel_too_long(9, None))));
    assert!(matches!(
        m.next_action(),
        Some(Fetch::GetChannelDifference { channel: 9, .. })
    ));
    let batches = m.feed_channel_difference(
        9,
        b(channel_difference(
            true,
            4,
            &Diff {
                msgs: std::slice::from_ref(&post),
                ..EMPTY_DIFF
            },
        )),
    );
    let want = tl::enums::Update::NewChannelMessage(tl::types::UpdateNewChannelMessage {
        message: typed(&post),
        pts: 4,
        pts_count: 0,
    })
    .to_bytes();
    assert_eq!(all_updates(&batches), vec![b(want)]);
}

// ----------------------------------------------------------- model checker

const SCRIPTS: u64 = 200;
const MAIN_EVENTS: usize = 24;
const SEED: u64 = 0x7467_696e_5f6d_3335;
const USERS: [i64; 5] = [100, 101, 102, 103, 104];
const KNOWN_USERS: [i64; 2] = [100, 101];
/// Channels 1–3 start known; channel 4 is unknown until its first update.
const CHANNELS: [i64; 4] = [1, 2, 3, 4];
const UNKNOWN_CHANNEL: i64 = 4;
const DIFF_LIMIT: usize = 3;
const CHANNEL_DIFF_LIMIT: usize = 2;

/// SplitMix64: a fixed seed gives the same scripts on every platform.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
    fn pick<T: Copy>(&mut self, from: &[T]) -> T {
        from[usize::try_from(self.below(from.len() as u64)).expect("index")]
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Msg { user: i64 },
    Enc,
    Affected { fed: bool },
    Chan(i64),
    Status,
}

/// One event of the server's log, with the state after it.
struct Event {
    kind: Kind,
    tag: u32,
    /// What the socket carries for it, if anything.
    push: Option<Bytes>,
    /// The message (or encrypted message) for differences; for `Affected`,
    /// its `updateDeleteMessages`.
    body: Vec<u8>,
    pts: i32,
    qts: i32,
    seq: i32,
    date: i32,
    /// The channel's pts after it (`Chan`).
    chan_pts: i32,
    /// The common pts when it happened (`Chan`).
    mark: i32,
}

struct Script {
    seed: u64,
    events: Vec<Event>,
    /// `[main, flush 1, flush 2]` boundaries.
    phases: [usize; 3],
    users: HashMap<i64, Vec<u8>>,
    chats: HashMap<i64, Vec<u8>>,
    /// Message bytes (and short message pushes) → event index.
    by_bytes: HashMap<Vec<u8>, usize>,
    initial: StoredState,
}

fn script(seed: u64) -> Script {
    let mut rng = Rng(seed);
    let initial = StoredState {
        common: UpdateState {
            pts: 10,
            qts: 5,
            seq: 3,
            date: 1,
        },
        channels: CHANNELS
            .iter()
            .filter(|&&c| c != UNKNOWN_CHANNEL)
            .map(|&c| (c, 5))
            .collect(),
        known_users: KNOWN_USERS.to_vec(),
    };
    let (mut pts, mut qts, mut seq) = (10, 5, 3);
    let mut chan: BTreeMap<i64, i32> = CHANNELS.iter().map(|&c| (c, 5)).collect();
    let users: HashMap<i64, Vec<u8>> = USERS.iter().map(|&u| (u, user(u))).collect();
    let chats: HashMap<i64, Vec<u8>> = CHANNELS.iter().map(|&c| (c, channel(c))).collect();
    let mut events = Vec::new();
    let mut by_bytes = HashMap::new();
    // Two flush rounds of one post per channel: the lazy channels catch up
    // on their next activity. The common box gets none; after a restart
    // it relies on the start-up difference alone.
    let total = MAIN_EVENTS + 2 * CHANNELS.len();
    let mut phases = [0; 3];
    for i in 0..total {
        let reliable = i >= MAIN_EVENTS;
        let flush = (i - MAIN_EVENTS.min(i)) % CHANNELS.len();
        let tag = u32::try_from(i).expect("tag") + 1;
        let date = i32::try_from(i).expect("date") + 2;
        let kind = if reliable {
            Kind::Chan(CHANNELS[flush])
        } else {
            match rng.below(100) {
                0..30 => Kind::Msg {
                    user: rng.pick(&USERS),
                },
                30..40 => Kind::Enc,
                40..48 => Kind::Affected {
                    fed: rng.chance(70),
                },
                48..85 => Kind::Chan(rng.pick(&CHANNELS)),
                _ => Kind::Status,
            }
        };
        let lost = !reliable && rng.chance(15);
        let mut chan_pts = 0;
        let mut mark = 0;
        let (push, body) = match kind {
            Kind::Msg { user } => {
                pts += 1;
                let m = message(tag, PeerId::User(user), &format!("e{tag}"));
                by_bytes.insert(m.clone(), i);
                let push = if rng.chance(50) {
                    let s = short_message(tag, user, pts, 1);
                    by_bytes.insert(s.clone(), i);
                    s
                } else {
                    seq += 1;
                    updates(
                        &[new_message(&m, pts, 1)],
                        &[users[&user].clone()],
                        NONE,
                        date,
                        seq,
                    )
                };
                (Some(push), m)
            }
            Kind::Enc => {
                qts += 1;
                seq += 1;
                let m = encrypted(tag);
                by_bytes.insert(m.clone(), i);
                (
                    Some(updates(&[new_encrypted(&m, qts)], NONE, NONE, date, seq)),
                    m,
                )
            }
            Kind::Affected { .. } => {
                pts += 2;
                let t = i32::try_from(tag).expect("tag");
                (None, delete_messages(&[t, t + 1], pts, 2))
            }
            Kind::Chan(c) => {
                let p = chan.entry(c).or_insert(5);
                *p += 1;
                chan_pts = *p;
                mark = pts;
                let m = message(tag, PeerId::Channel(c), &format!("e{tag}"));
                by_bytes.insert(m.clone(), i);
                let push = if !reliable && rng.chance(10) {
                    short(&channel_too_long(c, rng.chance(50).then_some(chan_pts)))
                } else {
                    seq += 1;
                    updates(
                        &[new_channel_message(&m, chan_pts, 1)],
                        NONE,
                        &[chats[&c].clone()],
                        date,
                        seq,
                    )
                };
                (Some(push), m)
            }
            Kind::Status => (Some(short(&user_status(rng.pick(&USERS)))), Vec::new()),
        };
        events.push(Event {
            kind,
            tag,
            push: push.filter(|_| !lost).map(Bytes::from),
            body,
            pts,
            qts,
            seq,
            date,
            chan_pts,
            mark,
        });
        if i + 1 == MAIN_EVENTS {
            phases[0] = i + 1;
        }
        if i + 1 == MAIN_EVENTS + CHANNELS.len() {
            phases[1] = i + 1;
        }
    }
    phases[2] = events.len();
    Script {
        seed,
        events,
        phases,
        users,
        chats,
        by_bytes,
        initial,
    }
}

/// What the caller persisted: the deltas `committed` returned and the
/// peers of admitted batches.
#[derive(Clone)]
struct Persisted {
    common: UpdateState,
    channels: BTreeMap<i64, i32>,
    known: BTreeSet<i64>,
}

impl Persisted {
    fn stored(&self) -> StoredState {
        StoredState {
            common: self.common,
            channels: self.channels.iter().map(|(&c, &p)| (c, p)).collect(),
            known_users: self.known.iter().copied().collect(),
        }
    }

    fn apply(&mut self, d: &StateDelta) {
        self.common.pts = d.pts.unwrap_or(self.common.pts);
        self.common.qts = d.qts.unwrap_or(self.common.qts);
        self.common.seq = d.seq.unwrap_or(self.common.seq);
        self.common.date = d.date.unwrap_or(self.common.date);
        for &(c, p) in &d.channels {
            self.channels.insert(c, p);
        }
    }
}

struct Sim<'a> {
    s: &'a Script,
    rng: Rng,
    m: UpdateManager,
    persisted: Persisted,
    net: VecDeque<Bytes>,
    in_flight: VecDeque<Fetch>,
    /// Events that have happened on the server.
    done: usize,
    admitted: HashSet<usize>,
    /// The first admitted update of the unknown channel: its pts before.
    first_unknown: Option<i32>,
    point: u64,
    kill_at: Option<u64>,
    killable: bool,
    kills: u32,
}

impl<'a> Sim<'a> {
    fn new(s: &'a Script, kill_at: Option<u64>) -> Self {
        let persisted = Persisted {
            common: s.initial.common,
            channels: s.initial.channels.iter().copied().collect(),
            known: s.initial.known_users.iter().copied().collect(),
        };
        Self {
            s,
            rng: Rng(s.seed ^ 0x5eed),
            m: UpdateManager::new(s.initial.clone()),
            persisted,
            net: VecDeque::new(),
            in_flight: VecDeque::new(),
            done: 0,
            admitted: HashSet::new(),
            first_unknown: None,
            point: 0,
            kill_at,
            killable: true,
            kills: 0,
        }
    }

    /// A kill point: true when the manager was killed here.
    fn tick(&mut self) -> bool {
        if !self.killable {
            return false;
        }
        self.point += 1;
        if Some(self.point) != self.kill_at {
            return false;
        }
        // The process dies: memory, the connection and its pushes, and
        // calls in flight are gone. Restart from what was persisted.
        self.kills += 1;
        self.m = UpdateManager::new(self.persisted.stored());
        self.net.clear();
        self.in_flight.clear();
        true
    }

    fn take(&mut self, batches: Vec<Batch>) {
        for batch in batches {
            if self.tick() {
                return; // before admission
            }
            // Admission may be full; a batch with no updates always fits.
            if !batch.updates.is_empty() && self.rng.chance(4) {
                if let Some(d) = self.m.refused(batch.id) {
                    self.persisted.apply(&d);
                }
                continue;
            }
            self.admit(&batch);
            if self.tick() {
                return; // admitted, not committed
            }
            if let Some(d) = self.m.committed(batch.id) {
                self.persisted.apply(&d);
            }
            if self.tick() {
                return; // committed
            }
        }
    }

    fn admit(&mut self, batch: &Batch) {
        for u in &batch.updates {
            if let Some(i) = self.identify(u.bytes()) {
                self.admitted.insert(i);
                let e = &self.s.events[i];
                if e.kind == Kind::Chan(UNKNOWN_CHANNEL) && self.first_unknown.is_none() {
                    self.first_unknown = Some(e.chan_pts - 1);
                }
            }
        }
        for p in &batch.peers {
            if matches!(p.kind, PeerKind::User { .. }) && p.access_hash.is_some() {
                self.persisted.known.insert(p.id);
            }
        }
    }

    /// The server event an update is, by its message bytes.
    fn identify(&self, u: &[u8]) -> Option<usize> {
        if let Some(&i) = self.s.by_bytes.get(u) {
            return Some(i);
        }
        let id = u32::from_le_bytes(u.get(..4)?.try_into().ok()?);
        let body = match id {
            UPDATE_NEW_MESSAGE | UPDATE_NEW_CHANNEL_MESSAGE => u.get(4..u.len().checked_sub(8)?)?,
            UPDATE_NEW_ENCRYPTED_MESSAGE => u.get(4..u.len().checked_sub(4)?)?,
            _ => return None,
        };
        self.s.by_bytes.get(body).copied()
    }

    fn pump(&mut self) {
        while let Some(f) = self.m.next_action() {
            self.in_flight.push_back(f);
        }
    }

    fn occur(&mut self, reliable: bool) {
        let e = &self.s.events[self.done];
        self.done += 1;
        if let Some(p) = &e.push {
            self.net.push_back(p.clone());
        }
        if let Kind::Affected { fed: true } = e.kind
            && !reliable
        {
            let batches = self.m.feed_affected(BoxKind::Common, e.pts, 2);
            self.take(batches);
        }
    }

    fn deliver(&mut self) {
        if self.net.len() >= 2 && self.rng.chance(8) {
            self.net.swap(0, 1);
        }
        let Some(p) = self.net.pop_front() else {
            return;
        };
        if self.rng.chance(5) {
            self.net.push_front(p.clone());
        }
        let batches = self.m.feed_push(p);
        self.take(batches);
    }

    fn answer(&mut self, f: Fetch) {
        let batches = match f {
            Fetch::GetState => {
                let (pts, qts, seq, date) = self.current();
                self.m.feed_state(b(state(pts, qts, date, seq)))
            }
            Fetch::GetDifference { pts, qts, .. } => {
                let d = self.difference(pts, qts);
                self.m.feed_difference(b(d))
            }
            Fetch::GetChannelDifference { channel, pts } => {
                let d = self.channel_difference(channel, pts);
                self.m.feed_channel_difference(channel, b(d))
            }
        };
        self.take(batches);
    }

    fn current(&self) -> (i32, i32, i32, i32) {
        match self.done.checked_sub(1).map(|i| &self.s.events[i]) {
            Some(e) => (e.pts, e.qts, e.seq, e.date),
            None => {
                let c = self.s.initial.common;
                (c.pts, c.qts, c.seq, c.date)
            }
        }
    }

    fn channel_pts(&self, c: i64) -> i32 {
        self.s.events[..self.done]
            .iter()
            .rev()
            .find(|e| e.kind == Kind::Chan(c))
            .map_or(5, |e| e.chan_pts)
    }

    fn difference(&self, pts: i32, qts: i32) -> Vec<u8> {
        let log = &self.s.events[..self.done];
        let pending: Vec<&Event> = log
            .iter()
            .filter(|e| match e.kind {
                Kind::Msg { .. } | Kind::Affected { .. } => e.pts > pts,
                Kind::Enc => e.qts > qts,
                _ => false,
            })
            .collect();
        let slice = pending.len() > DIFF_LIMIT;
        let taken = &pending[..pending.len().min(DIFF_LIMIT)];
        let (mut msgs, mut enc, mut other, mut users, mut chats) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for e in taken {
            match e.kind {
                Kind::Msg { user } => {
                    msgs.push(e.body.clone());
                    users.push(self.s.users[&user].clone());
                }
                Kind::Enc => enc.push(e.body.clone()),
                _ => other.push(e.body.clone()),
            }
        }
        if slice {
            let last = taken[taken.len() - 1];
            let d = Diff {
                msgs: &msgs,
                enc: &enc,
                other: &other,
                chats: &chats,
                users: &users,
            };
            return difference(&d, &state(last.pts, last.qts, last.date, last.seq), true);
        }
        // Channels with updates since that state are signalled.
        let since: BTreeSet<i64> = log
            .iter()
            .filter_map(|e| match e.kind {
                Kind::Chan(c) if e.mark >= pts => Some(c),
                _ => None,
            })
            .collect();
        for c in since {
            other.push(channel_too_long(c, Some(self.channel_pts(c))));
            chats.push(self.s.chats[&c].clone());
        }
        let (cp, cq, cs, cd) = self.current();
        if msgs.is_empty() && enc.is_empty() && other.is_empty() && (cp, cq) == (pts, qts) {
            return difference_empty(cd, cs);
        }
        let d = Diff {
            msgs: &msgs,
            enc: &enc,
            other: &other,
            chats: &chats,
            users: &users,
        };
        difference(&d, &state(cp, cq, cd, cs), false)
    }

    fn channel_difference(&self, c: i64, pts: i32) -> Vec<u8> {
        let pending: Vec<&Event> = self.s.events[..self.done]
            .iter()
            .filter(|e| e.kind == Kind::Chan(c) && e.chan_pts > pts)
            .collect();
        if pending.is_empty() {
            return channel_difference_empty(true, self.channel_pts(c));
        }
        let fin = pending.len() <= CHANNEL_DIFF_LIMIT;
        let taken = &pending[..pending.len().min(CHANNEL_DIFF_LIMIT)];
        let msgs: Vec<Vec<u8>> = taken.iter().map(|e| e.body.clone()).collect();
        let to = if fin {
            self.channel_pts(c)
        } else {
            taken[taken.len() - 1].chan_pts
        };
        let chats = [self.s.chats[&c].clone()];
        let d = Diff {
            msgs: &msgs,
            chats: &chats,
            ..EMPTY_DIFF
        };
        channel_difference(fin, to, &d)
    }

    async fn step(&mut self) {
        match self.rng.below(100) {
            0..30 => {
                if self.done < self.s.phases[0] {
                    self.occur(false);
                }
            }
            30..58 => self.deliver(),
            58..76 => {
                if let Some(f) = self.in_flight.pop_front() {
                    if self.rng.chance(5) {
                        self.m.fetch_failed(f);
                    } else {
                        self.answer(f);
                    }
                }
            }
            76..90 => advance(Duration::from_millis(100 + self.rng.below(600))).await,
            90..93 => {
                // The connection ends: its pushes and calls are lost, and
                // the sender adds `updatesTooLong`.
                self.net.clear();
                for f in std::mem::take(&mut self.in_flight) {
                    self.m.fetch_failed(f);
                }
                let batches = self.m.feed_push(b(T::new(UPDATES_TOO_LONG).done()));
                self.take(batches);
            }
            _ => {}
        }
        self.pump();
    }

    /// Delivers everything and answers every call, then moves the clock to
    /// the manager's next deadline, a few times over: a gap's hold, a
    /// channel's catch-up, and at last the idle difference, which finds a
    /// lost push that nothing after it revealed (Telegram's 15 minutes).
    async fn settle(&mut self) -> Result<(), String> {
        for round in 0..6 {
            let mut moves = 0;
            loop {
                self.pump();
                if let Some(p) = self.net.pop_front() {
                    let batches = self.m.feed_push(p);
                    self.take(batches);
                } else if let Some(f) = self.in_flight.pop_front() {
                    self.answer(f);
                } else {
                    break;
                }
                moves += 1;
                if moves > 500 {
                    return Err("did not settle".into());
                }
            }
            if round < 5 {
                let wait = self.m.deadline().saturating_duration_since(Instant::now());
                advance(wait + Duration::from_millis(1)).await;
            }
        }
        Ok(())
    }

    async fn run(mut self) -> (u64, Result<(), String>) {
        let mut steps = 0;
        while self.done < self.s.phases[0] {
            steps += 1;
            if steps > 10_000 {
                return (self.point, Err("main phase did not end".into()));
            }
            self.tick();
            self.step().await;
        }
        // The next activity in every channel, delivered: the lazy channels
        // catch up on it (decision 6). A kill may still land here, so a
        // second round follows with kills off.
        for phase in 1..3 {
            if phase == 2 {
                self.killable = false;
            }
            while self.done < self.s.phases[phase] {
                self.occur(true);
            }
            if let Err(e) = self.settle().await {
                return (self.point, Err(e));
            }
        }
        (self.point, self.check())
    }

    fn check(&self) -> Result<(), String> {
        for (i, e) in self.s.events.iter().enumerate() {
            let required = match e.kind {
                Kind::Msg { .. } | Kind::Enc => true,
                Kind::Chan(UNKNOWN_CHANNEL) => {
                    self.first_unknown.is_some_and(|base| e.chan_pts > base)
                }
                Kind::Chan(_) => true,
                Kind::Affected { .. } | Kind::Status => false,
            };
            if required && !self.admitted.contains(&i) {
                return Err(format!(
                    "event {i} ({:?}, tag {}) never admitted; kills {}",
                    e.kind, e.tag, self.kills
                ));
            }
        }
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn kill_at_every_point_loses_no_sequenced_update() {
    let mut seeds = Rng(SEED);
    let mut runs = 0u64;
    let mut killed_runs = 0u64;
    for n in 0..SCRIPTS {
        let s = script(seeds.next());
        let (points, clean) = Sim::new(&s, None).run().await;
        if let Err(e) = clean {
            panic!("script {n}, no kill: {e}");
        }
        runs += 1;
        for k in 1..=points {
            let sim = Sim::new(&s, Some(k));
            let (_, r) = sim.run().await;
            if let Err(e) = r {
                panic!("script {n}, kill at point {k} of {points}: {e}");
            }
            runs += 1;
            killed_runs += 1;
        }
    }
    assert!(
        killed_runs > SCRIPTS * 50,
        "{killed_runs} kill runs in {runs}"
    );
}
