// Copyright (c) 2004-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

use std::collections::HashMap;
use std::sync::Arc;

use ascii::AsciiString;
use failure::err_msg;
use failure::prelude::*;
use futures::{stream, prelude::*};
use futures_ext::{BoxFuture, FutureExt};
use slog::Logger;

use blobrepo::BlobRepo;
use bookmarks::Bookmark;
use mercurial::RevlogRepo;
use mercurial_types::HgChangesetId;

pub fn read_bookmarks(revlogrepo: RevlogRepo) -> BoxFuture<Vec<(Vec<u8>, HgChangesetId)>, Error> {
    let bookmarks = Arc::new(try_boxfuture!(revlogrepo.get_bookmarks()));

    (*bookmarks)
        .keys()
        .and_then({
            let bookmarks = bookmarks.clone();
            move |key| {
                (*bookmarks).get(&key).and_then(move |cs_id| {
                    cs_id
                        .ok_or_else(|| format_err!("Bookmark value missing: {:?}", key))
                        .map(move |(cs_id, _)| (key, cs_id))
                })
            }
        })
        .collect()
        .boxify()
}

pub fn upload_bookmarks(
    logger: &Logger,
    revlogrepo: RevlogRepo,
    blobrepo: Arc<BlobRepo>,
    stale_bookmarks: Vec<(Vec<u8>, HgChangesetId)>,
) -> BoxFuture<(), Error> {
    let logger = logger.clone();
    let stale_bookmarks = Arc::new(stale_bookmarks.into_iter().collect::<HashMap<_, _>>());

    read_bookmarks(revlogrepo)
        .map({
            cloned!(logger, blobrepo, stale_bookmarks);
            move |bookmarks| {
                stream::futures_unordered(bookmarks.into_iter().map(|(key, cs_id)| {
                    blobrepo
                        .changeset_exists(&cs_id)
                        .and_then({
                            cloned!(logger, key, blobrepo, stale_bookmarks);
                            move |exists| {
                                match (exists, stale_bookmarks.get(&key).cloned()) {
                                    (false, Some(stale_cs_id)) => {
                                        info!(
                                            logger,
                                            "current version of bookmark {:?} couldn't be \
                                            imported, because cs {:?} was not present in blobrepo \
                                            yet; using stale version instead {:?}",
                                            key,
                                            cs_id,
                                            stale_cs_id,
                                        );

                                        blobrepo
                                            .changeset_exists(&stale_cs_id)
                                            .map(move |exists| (key, stale_cs_id, exists))
                                            .boxify()
                                    }
                                    _ => Ok((key, cs_id, exists)).into_future().boxify(),
                                }
                            }})
                        .and_then({
                            cloned!(blobrepo, logger);
                            move |(key, cs_id, exists)| {
                                if exists {
                                    blobrepo.get_bonsai_from_hg(&cs_id)
                                        .and_then(move |bcs_id| bcs_id.ok_or(err_msg(
                                            format!("failed to resolve hg to bonsai: {}", cs_id),
                                        )))
                                        .map(move |bcs_id| Some((key, bcs_id)))
                                        .left_future()
                                } else {
                                    info!(
                                        logger,
                                        "did not update bookmark {:?}, because cs {:?} was not imported yet",
                                        key,
                                        cs_id,
                                    );
                                    Ok(None).into_future().right_future()
                                }
                            }
                        })
                }))
            }
        })
        .flatten_stream()
        .filter_map(|key_cs_id| key_cs_id)
        .chunks(100) // send 100 bookmarks in a single transaction
        .and_then({
            let blobrepo = blobrepo.clone();
            move |vec| {
                let count = vec.len();
                let mut transaction = blobrepo.update_bookmark_transaction();

                for (key, value) in vec {
                    let key = Bookmark::new_ascii(try_boxfuture!(AsciiString::from_ascii(key)));
                    try_boxfuture!(transaction.force_set(&key, &value))
                }

                transaction.commit()
                    .and_then(move |ok| {
                        if ok {
                            Ok(count)
                        } else {
                            Err(format_err!("Bookmark transaction failed"))
                        }
                    })
                    .boxify()
            }
        }).for_each(move |count| {
            info!(logger, "uploaded chunk of {:?} bookmarks", count);
            Ok(())
        }).boxify()
}
