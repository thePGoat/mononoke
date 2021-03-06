// Copyright (c) 2004-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

mod remotefilelog;
pub mod streaming_clone;

use std::collections::{HashMap, HashSet};
use std::iter::FromIterator;
use std::mem;
use std::str::FromStr;
use std::sync::Arc;

use bytes::{BufMut, Bytes, BytesMut};
use failure::err_msg;
use futures::{future, stream, Async, Future, IntoFuture, Poll, Stream, stream::empty};
use futures_ext::{select_all, BoxFuture, BoxStream, FutureExt, StreamExt};
use futures_stats::{Timed, TimedStreamTrait};
use itertools::Itertools;
use slog::Logger;
use stats::Histogram;
use time_ext::DurationExt;
use uuid::Uuid;

use blobrepo::HgBlobChangeset;
use bookmarks::Bookmark;
use bundle2_resolver;
use context::CoreContext;
use mercurial_bundles::{create_bundle_stream, parts, Bundle2Item};
use mercurial_types::{percent_encode, Entry, HgChangesetId, HgManifestId, HgNodeHash, MPath,
                      RepoPath, Type, NULL_HASH};
use mercurial_types::manifest_utils::{changed_entry_stream_with_pruner, CombinatorPruner,
                                      DeletedPruner, EntryStatus, FilePruner, Pruner,
                                      VisitedPruner};
use scuba_ext::{ScubaSampleBuilder, ScubaSampleBuilderExt};
use tracing::{TraceContext, Traced};

use blobrepo::BlobRepo;
use hgproto::{self, GetbundleArgs, GettreepackArgs, HgCommandRes, HgCommands};

use self::remotefilelog::create_remotefilelog_blob;
use self::streaming_clone::RevlogStreamingChunks;

use errors::*;
use hooks::HookManager;
use mononoke_repo::{MononokeRepo, MysqlStreamingCloneConfig};

const MAX_NODES_TO_LOG: usize = 5;

define_stats! {
    prefix = "mononoke.repo_client";
    getbundle_ms:
        histogram(500, 0, 10_000, AVG, SUM, COUNT; P 5; P 25; P 50; P 75; P 95; P 97; P 99),
    gettreepack_ms:
        histogram(500, 0, 20_000, AVG, SUM, COUNT; P 5; P 25; P 50; P 75; P 95; P 97; P 99),
    getfiles_ms:
        histogram(500, 0, 20_000, AVG, SUM, COUNT; P 5; P 25; P 50; P 75; P 95; P 97; P 99),
}

mod ops {
    pub static HELLO: &str = "hello";
    pub static UNBUNDLE: &str = "unbundle";
    pub static HEADS: &str = "heads";
    pub static LOOKUP: &str = "lookup";
    pub static LISTKEYS: &str = "listkeys";
    pub static KNOWN: &str = "known";
    pub static BETWEEN: &str = "between";
    pub static GETBUNDLE: &str = "getbundle";
    pub static GETTREEPACK: &str = "gettreepack";
    pub static GETFILES: &str = "getfiles";
}

fn format_nodes_list(mut nodes: Vec<HgNodeHash>) -> String {
    nodes.sort();
    nodes.into_iter().map(|node| format!("{}", node)).join(" ")
}

fn format_utf8_bytes_list(mut entries: Vec<Bytes>) -> String {
    entries.sort();
    entries
        .into_iter()
        .map(|entry| String::from_utf8_lossy(&entry).into_owned())
        .join(" ")
}

fn wireprotocaps() -> Vec<String> {
    vec![
        "lookup".to_string(),
        "known".to_string(),
        "getbundle".to_string(),
        "unbundle=HG10GZ,HG10BZ,HG10UN".to_string(),
        "gettreepack".to_string(),
        "remotefilelog".to_string(),
        "pushkey".to_string(),
        "stream-preferred".to_string(),
        "stream_option".to_string(),
        "streamreqs=generaldelta,lz4revlog,revlogv1".to_string(),
    ]
}

fn bundle2caps() -> String {
    let caps = vec![
        ("HG20", vec![]),
        // Note that "listkeys" is *NOT* returned as a bundle2 capability; that's because there's
        // a race that can happen. Here's how:
        // 1. The client does discovery to figure out which heads are missing.
        // 2. At this point, a frequently updated bookmark (say "master") moves forward.
        // 3. The client requests the heads discovered in step 1 + the latest value of master.
        // 4. The server returns changesets up to those heads, plus the latest version of master.
        //
        // master doesn't point to a commit that will exist on the client at the end of the pull,
        // so the client ignores it.
        //
        // The workaround here is to force bookmarks to be sent before discovery happens. Disabling
        // the listkeys capabilities causes the Mercurial client to do that.
        //
        // A better fix might be to snapshot and maintain the bookmark state on the server at the
        // start of discovery.
        //
        // The best fix here would be to change the protocol to represent bookmark pulls
        // atomically.
        //
        // Some other notes:
        // * Stock Mercurial doesn't appear to have this problem. @rain1 hasn't verified why, but
        //   believes it's because bookmarks get loaded up into memory before discovery and then
        //   don't get reloaded for the duration of the process. (In Mononoke, this is the
        //   "snapshot and maintain the bookmark state" approach mentioned above.)
        // * There's no similar race with pushes updating bookmarks, so "pushkey" is still sent
        //   as a capability.
        // * To repro the race, run test-bookmark-race.t with the following line enabled.

        // ("listkeys", vec![]),
        ("changegroup", vec!["02"]),
        ("b2x:infinitepush", vec![]),
        ("b2x:infinitepushscratchbookmarks", vec![]),
        ("pushkey", vec![]),
        ("treemanifestserver", vec!["True"]),
        ("b2x:rebase", vec![]),
        ("b2x:rebasepackpart", vec![]),
    ];

    let mut encodedcaps = vec![];

    for &(ref key, ref value) in &caps {
        let encodedkey = key.to_string();
        if value.len() > 0 {
            let encodedvalue = value.join(",");
            encodedcaps.push([encodedkey, encodedvalue].join("="));
        } else {
            encodedcaps.push(encodedkey)
        }
    }

    percent_encode(&encodedcaps.join("\n"))
}

#[derive(Clone)]
pub struct RepoClient {
    repo: MononokeRepo,
    ctxt: CoreContext<Uuid>,
}

impl RepoClient {
    pub fn new(repo: MononokeRepo, ctxt: CoreContext<Uuid>) -> Self {
        RepoClient { repo, ctxt }
    }

    fn logger(&self) -> &Logger {
        self.ctxt.logger()
    }

    fn trace(&self) -> &TraceContext {
        self.ctxt.trace()
    }

    fn scuba_logger(&self, op: &str, args: Option<String>) -> ScubaSampleBuilder {
        let mut scuba_logger = self.ctxt.scuba().clone();

        scuba_logger.add("command", op);

        if let Some(args) = args {
            scuba_logger.add("command_args", args);
        }

        scuba_logger.log_with_msg("Start processing", None);
        scuba_logger
    }

    fn create_bundle(&self, args: GetbundleArgs) -> Result<BoxStream<Bytes, Error>> {
        let blobrepo = self.repo.blobrepo();
        let mut bundle2_parts = vec![];
        let cg_part_builder = bundle2_resolver::create_getbundle_response(
            blobrepo.clone(),
            args.common
                .into_iter()
                .map(|head| HgChangesetId::new(head))
                .collect(),
            args.heads
                .into_iter()
                .map(|head| HgChangesetId::new(head))
                .collect(),
        )?;
        bundle2_parts.push(cg_part_builder);

        // XXX Note that listkeys is NOT returned as a bundle2 capability -- see comment in
        // bundle2caps() for why.

        // TODO: generalize this to other listkey types
        // (note: just calling &b"bookmarks"[..] doesn't work because https://fburl.com/0p0sq6kp)
        if args.listkeys.contains(&b"bookmarks".to_vec()) {
            let items = blobrepo.get_bookmarks().map(|(name, cs)| {
                let hash: Vec<u8> = cs.into_nodehash().to_hex().into();
                (name.to_string(), hash)
            });
            bundle2_parts.push(parts::listkey_part("bookmarks", items)?);
        }
        // TODO(stash): handle includepattern= and excludepattern=

        let compression = None;
        Ok(create_bundle_stream(bundle2_parts, compression).boxify())
    }

    fn gettreepack_untimed(&self, params: GettreepackArgs) -> BoxStream<Bytes, Error> {
        debug!(self.logger(), "gettreepack");

        // 65536 matches the default TREE_DEPTH_MAX value from Mercurial
        let fetchdepth = params.depth.unwrap_or(2 << 16);

        if !params.directories.is_empty() {
            // This param is not used by core hg, don't worry about implementing it now
            return stream::once(Err(err_msg("directories param is not supported"))).boxify();
        }

        // TODO(stash): T25850889 only one basemfnodes is used. That means that trees that client
        // already has can be sent to the client.
        let basemfnode = params.basemfnodes.get(0).cloned().unwrap_or(NULL_HASH);

        let rootpath = if params.rootdir.is_empty() {
            None
        } else {
            Some(try_boxstream!(MPath::new(params.rootdir)))
        };

        let default_pruner = CombinatorPruner::new(FilePruner, DeletedPruner);

        let changed_entries = if params.mfnodes.len() > 1 {
            let visited_pruner = VisitedPruner::new();
            select_all(params.mfnodes.iter().map(|manifest_id| {
                get_changed_manifests_stream(
                    self.repo.blobrepo(),
                    &manifest_id,
                    &basemfnode,
                    rootpath.clone(),
                    CombinatorPruner::new(default_pruner.clone(), visited_pruner.clone()),
                    fetchdepth,
                    self.trace().clone(),
                )
            })).boxify()
        } else {
            match params.mfnodes.get(0) {
                Some(mfnode) => get_changed_manifests_stream(
                    self.repo.blobrepo(),
                    &mfnode,
                    &basemfnode,
                    rootpath.clone(),
                    default_pruner,
                    fetchdepth,
                    self.trace().clone(),
                ),
                None => empty().boxify(),
            }
        };

        let changed_entries = changed_entries
            .filter({
                let mut used_hashes = HashSet::new();
                move |entry| used_hashes.insert(*entry.0.get_hash())
            })
            .map({
                let blobrepo = self.repo.blobrepo().clone();
                let trace = self.trace().clone();
                move |(entry, basepath)| {
                    fetch_treepack_part_input(&blobrepo, entry, basepath, trace.clone())
                }
            });

        let part = parts::treepack_part(changed_entries);
        // Mercurial currently hangs while trying to read compressed bundles over the wire:
        // https://bz.mercurial-scm.org/show_bug.cgi?id=5646
        // TODO: possibly enable compression support once this is fixed.
        let compression = None;
        part.into_future()
            .map(move |part| create_bundle_stream(vec![part], compression))
            .flatten_stream()
            .boxify()
    }
}

impl HgCommands for RepoClient {
    // @wireprotocommand('between', 'pairs')
    fn between(&self, pairs: Vec<(HgNodeHash, HgNodeHash)>) -> HgCommandRes<Vec<Vec<HgNodeHash>>> {
        info!(self.logger(), "between pairs {:?}", pairs);

        struct ParentStream<CS> {
            repo: MononokeRepo,
            n: HgNodeHash,
            bottom: HgNodeHash,
            wait_cs: Option<CS>,
        };

        impl<CS> ParentStream<CS> {
            fn new(repo: &MononokeRepo, top: HgNodeHash, bottom: HgNodeHash) -> Self {
                ParentStream {
                    repo: repo.clone(),
                    n: top,
                    bottom: bottom,
                    wait_cs: None,
                }
            }
        }

        impl Stream for ParentStream<BoxFuture<HgBlobChangeset, hgproto::Error>> {
            type Item = HgNodeHash;
            type Error = hgproto::Error;

            fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
                if self.n == self.bottom || self.n == NULL_HASH {
                    return Ok(Async::Ready(None));
                }

                self.wait_cs = self.wait_cs.take().or_else(|| {
                    Some(
                        self.repo
                            .blobrepo()
                            .get_changeset_by_changesetid(&HgChangesetId::new(self.n)),
                    )
                });
                let cs = try_ready!(self.wait_cs.as_mut().unwrap().poll());
                self.wait_cs = None; // got it

                let p = cs.p1().cloned().unwrap_or(NULL_HASH);
                let prev_n = mem::replace(&mut self.n, p);

                Ok(Async::Ready(Some(prev_n)))
            }
        }

        let mut scuba_logger = self.scuba_logger(ops::BETWEEN, None);

        // TODO(jsgf): do pairs in parallel?
        // TODO: directly return stream of streams
        let repo = self.repo.clone();
        stream::iter_ok(pairs.into_iter())
            .and_then(move |(top, bottom)| {
                let mut f = 1;
                ParentStream::new(&repo, top, bottom)
                    .enumerate()
                    .filter(move |&(i, _)| {
                        if i == f {
                            f *= 2;
                            true
                        } else {
                            false
                        }
                    })
                    .map(|(_, v)| v)
                    .collect()
            })
            .collect()
            .traced(self.trace(), ops::BETWEEN, trace_args!())
            .timed(move |stats, _| {
                scuba_logger
                    .add_future_stats(&stats)
                    .log_with_msg("Command processed", None);
                Ok(())
            })
            .boxify()
    }

    // @wireprotocommand('heads')
    fn heads(&self) -> HgCommandRes<HashSet<HgNodeHash>> {
        // Get a stream of heads and collect them into a HashSet
        // TODO: directly return stream of heads
        let mut scuba_logger = self.scuba_logger(ops::HEADS, None);

        self.repo
            .blobrepo()
            .get_heads()
            .collect()
            .map(|v| v.into_iter().collect())
            .from_err()
            .traced(self.trace(), ops::HEADS, trace_args!())
            .timed(move |stats, _| {
                scuba_logger
                    .add_future_stats(&stats)
                    .log_with_msg("Command processed", None);
                Ok(())
            })
            .boxify()
    }

    // @wireprotocommand('lookup', 'key')
    fn lookup(&self, key: String) -> HgCommandRes<Bytes> {
        info!(self.logger(), "lookup: {:?}", key);
        // TODO(stash): T25928839 lookup should support prefixes
        let repo = self.repo.blobrepo().clone();
        let mut scuba_logger = self.scuba_logger(ops::LOOKUP, None);

        fn generate_resp_buf(success: bool, message: &[u8]) -> Bytes {
            let mut buf = BytesMut::with_capacity(message.len() + 3);
            if success {
                buf.put(b'1');
            } else {
                buf.put(b'0');
            }
            buf.put(b' ');
            buf.put(message);
            buf.put(b'\n');
            buf.freeze()
        }

        fn check_bookmark_exists(repo: BlobRepo, bookmark: Bookmark) -> HgCommandRes<Bytes> {
            repo.get_bookmark(&bookmark)
                .map(move |csid| match csid {
                    Some(csid) => generate_resp_buf(true, csid.to_hex().as_bytes()),
                    None => generate_resp_buf(false, format!("{} not found", bookmark).as_bytes()),
                })
                .boxify()
        }

        let node = HgNodeHash::from_str(&key).ok();
        let bookmark = Bookmark::new(&key).ok();

        let lookup_fut = match (node, bookmark) {
            (Some(node), Some(bookmark)) => {
                let csid = HgChangesetId::new(node);
                repo.changeset_exists(&csid)
                    .and_then(move |exists| {
                        if exists {
                            Ok(generate_resp_buf(true, node.to_hex().as_bytes()))
                                .into_future()
                                .boxify()
                        } else {
                            check_bookmark_exists(repo, bookmark)
                        }
                    })
                    .boxify()
            }
            (None, Some(bookmark)) => check_bookmark_exists(repo, bookmark),
            // Failed to parse as a hash or bookmark.
            _ => Ok(generate_resp_buf(false, "invalid input".as_bytes()))
                .into_future()
                .boxify(),
        };

        lookup_fut
            .traced(self.trace(), ops::LOOKUP, trace_args!())
            .timed(move |stats, _| {
                scuba_logger
                    .add_future_stats(&stats)
                    .log_with_msg("Command processed", None);
                Ok(())
            })
            .boxify()
    }

    // @wireprotocommand('known', 'nodes *'), but the '*' is ignored
    fn known(&self, nodes: Vec<HgNodeHash>) -> HgCommandRes<Vec<bool>> {
        if nodes.len() > MAX_NODES_TO_LOG {
            info!(self.logger(), "known: {:?}...", &nodes[..MAX_NODES_TO_LOG]);
        } else {
            info!(self.logger(), "known: {:?}", nodes);
        }
        let blobrepo = self.repo.blobrepo().clone();

        let mut scuba_logger = self.scuba_logger(ops::KNOWN, None);

        future::join_all(
            nodes
                .into_iter()
                .map(move |node| blobrepo.changeset_exists(&HgChangesetId::new(node))),
        ).traced(self.trace(), ops::KNOWN, trace_args!())
            .timed(move |stats, _| {
                scuba_logger
                    .add_future_stats(&stats)
                    .log_with_msg("Command processed", None);
                Ok(())
            })
            .boxify()
    }

    // @wireprotocommand('getbundle', '*')
    fn getbundle(&self, args: GetbundleArgs) -> BoxStream<Bytes, Error> {
        info!(self.logger(), "Getbundle: {:?}", args);

        let mut scuba_logger = self.scuba_logger(ops::GETBUNDLE, None);

        match self.create_bundle(args) {
            Ok(res) => res.boxify(),
            Err(err) => stream::once(Err(err)).boxify(),
        }.traced(self.trace(), ops::GETBUNDLE, trace_args!())
            .timed(move |stats, _| {
                STATS::getbundle_ms.add_value(stats.completion_time.as_millis_unchecked() as i64);
                scuba_logger
                    .add_stream_stats(&stats)
                    .log_with_msg("Command processed", None);
                Ok(())
            })
            .boxify()
    }

    // @wireprotocommand('hello')
    fn hello(&self) -> HgCommandRes<HashMap<String, Vec<String>>> {
        info!(self.logger(), "Hello -> capabilities");

        let mut res = HashMap::new();
        let mut caps = wireprotocaps();
        caps.push(format!("bundle2={}", bundle2caps()));
        res.insert("capabilities".to_string(), caps);

        let mut scuba_logger = self.scuba_logger(ops::HELLO, None);

        future::ok(res)
            .traced(self.trace(), ops::HELLO, trace_args!())
            .timed(move |stats, _| {
                scuba_logger
                    .add_future_stats(&stats)
                    .log_with_msg("Command processed", None);
                Ok(())
            })
            .boxify()
    }

    // @wireprotocommand('listkeys', 'namespace')
    fn listkeys(&self, namespace: String) -> HgCommandRes<HashMap<Vec<u8>, Vec<u8>>> {
        if namespace == "bookmarks" {
            let mut scuba_logger = self.scuba_logger(ops::LISTKEYS, None);

            self.repo
                .blobrepo()
                .get_bookmarks()
                .map(|(name, cs)| {
                    let hash: Vec<u8> = cs.into_nodehash().to_hex().into();
                    (name, hash)
                })
                .collect()
                .map(|bookmarks| {
                    let bookiter = bookmarks
                        .into_iter()
                        .map(|(name, value)| (Vec::from(name.to_string()), value));
                    HashMap::from_iter(bookiter)
                })
                .traced(self.trace(), ops::LISTKEYS, trace_args!())
                .timed(move |stats, _| {
                    scuba_logger
                        .add_future_stats(&stats)
                        .log_with_msg("Command processed", None);
                    Ok(())
                })
                .boxify()
        } else {
            info!(
                self.logger(),
                "unsupported listkeys namespace: {}",
                namespace
            );
            future::ok(HashMap::new()).boxify()
        }
    }

    // @wireprotocommand('unbundle')
    fn unbundle(
        &self,
        heads: Vec<String>,
        stream: BoxStream<Bundle2Item, Error>,
        hook_manager: Arc<HookManager>,
    ) -> HgCommandRes<Bytes> {
        let mut scuba_logger = self.scuba_logger(ops::UNBUNDLE, None);

        let res = bundle2_resolver::resolve(
            Arc::new(self.repo.blobrepo().clone()),
            self.logger().new(o!("command" => "unbundle")),
            scuba_logger.clone(),
            self.repo.pushrebase_params().clone(),
            heads,
            stream,
            hook_manager,
        );

        res.traced(self.trace(), ops::UNBUNDLE, trace_args!())
            .timed(move |stats, _| {
                scuba_logger
                    .add_future_stats(&stats)
                    .log_with_msg("Command processed", None);
                Ok(())
            })
            .boxify()
    }

    // @wireprotocommand('gettreepack', 'rootdir mfnodes basemfnodes directories')
    fn gettreepack(&self, params: GettreepackArgs) -> BoxStream<Bytes, Error> {
        let args = format!(
            "rootdir: {}, mfnodes: {}, basemfnodes: {}, directories: {}",
            String::from_utf8_lossy(&params.rootdir),
            format_nodes_list(params.mfnodes.clone()),
            format_nodes_list(params.basemfnodes.clone()),
            format_utf8_bytes_list(params.directories.clone()),
        );

        let mut scuba_logger = self.scuba_logger(ops::GETTREEPACK, Some(args));

        self.gettreepack_untimed(params)
            .traced(self.trace(), ops::GETTREEPACK, trace_args!())
            .timed(move |stats, _| {
                STATS::gettreepack_ms.add_value(stats.completion_time.as_millis_unchecked() as i64);
                scuba_logger
                    .add_stream_stats(&stats)
                    .log_with_msg("Command processed", None);
                Ok(())
            })
            .boxify()
    }

    // @wireprotocommand('getfiles', 'files*')
    fn getfiles(&self, params: BoxStream<(HgNodeHash, MPath), Error>) -> BoxStream<Bytes, Error> {
        let logger = self.logger().clone();
        let trace = self.trace().clone();
        info!(logger, "getfiles");

        let this = self.clone();
        let getfiles_buffer_size = 100; // TODO(stash): make it configurable
        params
            .map(move |(node, path)| {
                let args = format!("node: {}, path: {}", node, path);
                let mut scuba_logger = this.scuba_logger(ops::GETFILES, Some(args));

                let repo = this.repo.clone();
                create_remotefilelog_blob(
                    Arc::new(repo.blobrepo().clone()),
                    node,
                    path.clone(),
                    trace.clone(),
                ).traced(
                    this.trace(),
                    ops::GETFILES,
                    trace_args!("node" => node.to_string(), "path" =>  path.to_string()),
                )
                    .timed(move |stats, _| {
                        STATS::getfiles_ms
                            .add_value(stats.completion_time.as_millis_unchecked() as i64);
                        scuba_logger
                            .add_future_stats(&stats)
                            .log_with_msg("Command processed", None);
                        Ok(())
                    })
            })
            .buffered(getfiles_buffer_size)
            .boxify()
    }

    // @wireprotocommand('stream_out_shallow')
    fn stream_out_shallow(&self) -> BoxStream<Bytes, Error> {
        info!(self.logger(), "stream_out_shallow");
        let changelog = match self.repo.streaming_clone() {
            None => Ok(RevlogStreamingChunks::new()).into_future().left_future(),
            Some(MysqlStreamingCloneConfig {
                blobstore,
                fetcher,
                repoid,
            }) => fetcher
                .fetch_changelog(*repoid, blobstore.clone())
                .right_future(),
        };

        changelog
            .map({
                let logger = self.logger().clone();
                move |changelog_chunks| {
                    debug!(
                        logger,
                        "streaming changelog {} index bytes, {} data bytes",
                        changelog_chunks.index_size,
                        changelog_chunks.data_size
                    );
                    let mut response_header = Vec::new();
                    // TODO(t34058163): actually send a real streaming response, not an empty one
                    // Send OK response.
                    response_header.push(Bytes::from_static(b"0\n"));
                    // send header.
                    let total_size = changelog_chunks.index_size + changelog_chunks.data_size;
                    let file_count = 2;
                    let header = format!("{} {}\n", file_count, total_size);
                    response_header.push(header.into_bytes().into());
                    let response = stream::iter_ok(response_header);

                    fn build_file_stream(
                        name: &str,
                        size: usize,
                        data: Vec<BoxFuture<Bytes, Error>>,
                    ) -> impl Stream<Item = Bytes, Error = Error> + Send {
                        let header = format!("{}\0{}\n", name, size);

                        stream::once(Ok(header.into_bytes().into()))
                            .chain(stream::iter_ok(data.into_iter()).buffered(100))
                    }

                    response
                        .chain(build_file_stream(
                            "00changelog.i",
                            changelog_chunks.index_size,
                            changelog_chunks.index_blobs,
                        ))
                        .chain(build_file_stream(
                            "00changelog.d",
                            changelog_chunks.data_size,
                            changelog_chunks.data_blobs,
                        ))
                }
            })
            .flatten_stream()
            .boxify()
    }
}

fn get_changed_manifests_stream(
    repo: &BlobRepo,
    mfid: &HgNodeHash,
    basemfid: &HgNodeHash,
    rootpath: Option<MPath>,
    pruner: impl Pruner + Send + Clone + 'static,
    max_depth: usize,
    trace: TraceContext,
) -> BoxStream<(Box<Entry + Sync>, Option<MPath>), Error> {
    let mfid = HgManifestId::new(*mfid);
    let manifest = repo.get_manifest_by_nodeid(&mfid)
        .traced(&trace, "fetch rootmf", trace_args!());
    let basemfid = HgManifestId::new(*basemfid);
    let basemanifest =
        repo.get_manifest_by_nodeid(&basemfid)
            .traced(&trace, "fetch baserootmf", trace_args!());

    let root_entry_stream = stream::once(Ok((repo.get_root_entry(&mfid), rootpath.clone())));

    if max_depth == 1 {
        return root_entry_stream.boxify();
    }

    let changed_entries = manifest
        .join(basemanifest)
        .map({
            let rootpath = rootpath.clone();
            move |(mf, basemf)| {
                changed_entry_stream_with_pruner(&mf, &basemf, rootpath, pruner, Some(max_depth))
            }
        })
        .flatten_stream();

    let changed_entries = changed_entries.map(move |entry_status| match entry_status.status {
        EntryStatus::Added(to_entry) | EntryStatus::Modified { to_entry, .. } => {
            assert!(
                to_entry.get_type() == Type::Tree,
                "FilePruner should have removed file entries"
            );
            (to_entry, entry_status.dirname)
        }
        EntryStatus::Deleted(..) => {
            panic!("DeletedPruner should have removed deleted entries");
        }
    });

    // Append root manifest as well
    changed_entries.chain(root_entry_stream).boxify()
}

fn fetch_treepack_part_input(
    repo: &BlobRepo,
    entry: Box<Entry + Sync>,
    basepath: Option<MPath>,
    trace: TraceContext,
) -> BoxFuture<parts::TreepackPartInput, Error> {
    let path = MPath::join_element_opt(basepath.as_ref(), entry.get_name());
    let repo_path = match path {
        Some(path) => {
            if entry.get_type() == Type::Tree {
                RepoPath::DirectoryPath(path)
            } else {
                RepoPath::FilePath(path)
            }
        }
        None => RepoPath::RootPath,
    };

    let node = entry.get_hash().clone();
    let path = repo_path.clone();

    let parents = entry.get_parents().traced(
        &trace,
        "fetching parents",
        trace_args!(
            "node" => node.to_string(),
            "path" => path.to_string()
        ),
    );

    let linknode_fut = repo.get_linknode(&repo_path, &entry.get_hash().into_nodehash())
        .traced(
            &trace,
            "fetching linknode",
            trace_args!(
                "node" => node.to_string(),
                "path" => path.to_string()
            ),
        );

    let content_fut = entry
        .get_raw_content()
        .map(|blob| blob.into_inner())
        .traced(
            &trace,
            "fetching raw content",
            trace_args!(
                "node" => node.to_string(),
                "path" => path.to_string()
            ),
        );

    parents
        .join(linknode_fut)
        .join(content_fut)
        .map(move |((parents, linknode), content)| {
            let (p1, p2) = parents.get_nodes();
            parts::TreepackPartInput {
                node: node.into_nodehash(),
                p1: p1.cloned(),
                p2: p2.cloned(),
                content,
                name: entry.get_name().cloned(),
                linknode: linknode.into_nodehash(),
                basepath,
            }
        })
        .boxify()
}
