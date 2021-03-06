// Copyright (c) 2004-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

//! This sub module contains a simple Rust implementation of hooks
//! This implementation is meant for testing and experimentation, not for real usage

#![deny(warnings)]

use super::{Hook, HookChangeset, HookContext, HookExecution};
use failure::Error;
use futures::finished;
use futures_ext::{BoxFuture, FutureExt};

pub struct RustHook {
    pub name: String,
}

impl Hook<HookChangeset> for RustHook {
    fn run(&self, _context: HookContext<HookChangeset>) -> BoxFuture<HookExecution, Error> {
        finished(HookExecution::Accepted).boxify()
    }
}
