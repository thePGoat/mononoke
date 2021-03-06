// Copyright (c) 2004-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

#![deny(warnings)]
#![feature(try_from, never_type)]
// FIXME T34253207, remove when https://github.com/diesel-rs/diesel/issues/1785 fixed
#![allow(proc_macro_derive_resolution_fallback)]

//! State for a single source control Repo

extern crate bytes;
#[macro_use]
extern crate cloned;
extern crate db_conn;
#[macro_use]
extern crate diesel;
#[macro_use]
extern crate failure_ext as failure;
#[macro_use]
extern crate futures;
#[macro_use]
extern crate futures_ext;
extern crate futures_stats;
extern crate itertools;
#[macro_use]
extern crate lazy_static;
extern crate pylz4;
extern crate rand;
extern crate scribe_cxx;
#[macro_use]
extern crate slog;
#[macro_use]
extern crate stats;
extern crate time_ext;
#[macro_use]
extern crate tracing;
extern crate uuid;

extern crate blobrepo;
extern crate blobstore;
extern crate bookmarks;
extern crate bundle2_resolver;
extern crate context;
extern crate filenodes;
extern crate hgproto;
extern crate hooks;
extern crate mercurial;
extern crate mercurial_bundles;
extern crate mercurial_types;
extern crate metaconfig;
extern crate mononoke_types;
extern crate revset;
extern crate scuba_ext;

mod client;
mod errors;
mod mononoke_repo;

pub use client::RepoClient;
pub use client::streaming_clone::MysqlStreamingChunksFetcher;
pub use mononoke_repo::{open_blobrepo, streaming_clone, MononokeRepo};
