// Copyright (c) 2004-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

use std::sync::Arc;
use std::usize;

use asyncmemo::{Asyncmemo, Filler};
use failure::Error;
use futures::Future;
use futures_ext::{BoxFuture, BoxStream, FutureExt};
use mercurial_types::{HgFileNodeId, RepoPath, RepositoryId};
use rust_thrift::compact_protocol;
use stats::Histogram;

use {thrift, FilenodeInfo, Filenodes};

define_stats! {
    prefix = "mononoke.filenodes";
    gaf_compact_bytes: histogram(
        "get_all_filenodes.thrift_compact.bytes";
        500, 0, 1_000_000, AVG, SUM, COUNT; P 50; P 95; P 99
    ),
}

pub struct CachingFilenodes {
    filenodes: Arc<Filenodes>,
    cache: Asyncmemo<FilenodesFiller>,
}

impl CachingFilenodes {
    pub fn new(filenodes: Arc<Filenodes>, sizelimit: usize) -> Self {
        let cache = Asyncmemo::with_limits(
            "filenodes",
            FilenodesFiller::new(filenodes.clone()),
            usize::MAX,
            sizelimit,
        );
        Self { filenodes, cache }
    }
}

impl Filenodes for CachingFilenodes {
    fn add_filenodes(
        &self,
        info: BoxStream<FilenodeInfo, Error>,
        repo_id: &RepositoryId,
    ) -> BoxFuture<(), Error> {
        self.filenodes.add_filenodes(info, repo_id)
    }

    fn get_filenode(
        &self,
        path: &RepoPath,
        filenode: &HgFileNodeId,
        repo_id: &RepositoryId,
    ) -> BoxFuture<Option<FilenodeInfo>, Error> {
        self.cache
            .get((path.clone(), *filenode, *repo_id))
            .then(|val| match val {
                Ok(val) => Ok(Some(val)),
                Err(Some(err)) => Err(err),
                Err(None) => Ok(None),
            })
            .boxify()
    }

    fn get_all_filenodes(
        &self,
        path: &RepoPath,
        repo_id: &RepositoryId,
    ) -> BoxFuture<Vec<FilenodeInfo>, Error> {
        self.filenodes
            .get_all_filenodes(path, repo_id)
            .inspect(|all_filenodes| {
                let all_filenodes = thrift::FilenodeInfoList::Data(
                    all_filenodes
                        .into_iter()
                        .map(|filenode_info| filenode_info.clone().into_thrift())
                        .collect(),
                );

                let serialized = compact_protocol::serialize(&all_filenodes);

                STATS::gaf_compact_bytes.add_value(serialized.len() as i64);
            })
            .boxify()
    }
}

pub struct FilenodesFiller {
    filenodes: Arc<Filenodes>,
}

impl FilenodesFiller {
    fn new(filenodes: Arc<Filenodes>) -> Self {
        FilenodesFiller { filenodes }
    }
}

impl Filler for FilenodesFiller {
    type Key = (RepoPath, HgFileNodeId, RepositoryId);
    type Value = Box<Future<Item = FilenodeInfo, Error = Option<Error>> + Send>;

    fn fill(
        &self,
        _cache: &Asyncmemo<Self>,
        &(ref path, ref filenode, ref repo_id): &Self::Key,
    ) -> Self::Value {
        self.filenodes
            .get_filenode(path, filenode, repo_id)
            .map_err(|err| Some(err))
            .and_then(|res| match res {
                Some(val) => Ok(val),
                None => Err(None),
            })
            .boxify()
    }
}