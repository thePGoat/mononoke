// Copyright (c) 2004-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

use clap::{App, Arg, ArgMatches, SubCommand};
use failure::Error;
use futures::{future, Future};
use futures_ext::{BoxFuture, FutureExt};
use serde_json::to_string_pretty;
use slog::Logger;

use blobrepo::BlobRepo;
use bookmarks::Bookmark;

const SET_CMD: &'static str = "set";
const GET_CMD: &'static str = "get";

pub fn prepare_command<'a, 'b>(app: App<'a, 'b>) -> App<'a, 'b> {
    let set = SubCommand::with_name(SET_CMD)
        .about(
            "sets a bookmark to a specific hg changeset, if the bookmark does not exist it will
                be created",
        )
        .args_from_usage(
            "<BOOKMARK_NAME>        'bookmark to target'
             <HG_CHANGESET_ID>      'revision to which the bookmark should point to'",
        );

    let get = SubCommand::with_name(GET_CMD)
        .about("gets the changeset of a specific bookmark")
        .args_from_usage(
            r#"
            <BOOKMARK_NAME>        'bookmark to target'
            --json                 'if provided json will be returned'
            "#,
        )
        .arg(
            Arg::with_name("changeset-type")
                .long("changeset-type")
                .short("cs")
                .takes_value(true)
                .possible_values(&["bonsai", "hg"])
                .required(false)
                .help("What changeset type to return, either bonsai or hg. Defaults to hg."),
        );

    app.about("set of commands to manipulate bookmarks")
        .subcommand(set)
        .subcommand(get)
}

pub fn handle_command<'a>(
    repo: &BlobRepo,
    matches: &ArgMatches<'a>,
    logger: Logger,
) -> BoxFuture<(), Error> {
    match matches.subcommand() {
        (GET_CMD, Some(sub_m)) => handle_get(sub_m, logger, repo.clone()),
        (SET_CMD, Some(sub_m)) => handle_set(sub_m, logger, repo.clone()),
        _ => {
            println!("{}", matches.usage());
            ::std::process::exit(1);
        }
    }
}

fn format_output(json_flag: bool, changeset_id: String, changeset_type: &str) -> String {
    if json_flag {
        let answer = json!({
            "changeset_type": changeset_type,
            "changeset_id": changeset_id
        });
        to_string_pretty(&answer).unwrap()
    } else {
        format!("({}) {}", changeset_type.to_uppercase(), changeset_id)
    }
}

fn handle_get<'a>(args: &ArgMatches<'a>, _logger: Logger, repo: BlobRepo) -> BoxFuture<(), Error> {
    let bookmark_name = args.value_of("BOOKMARK_NAME").unwrap().to_string();
    let bookmark = Bookmark::new(bookmark_name).unwrap();
    let changeset_type = args.value_of("changeset-type").unwrap_or("hg");
    let json_flag: bool = args.is_present("json");

    match changeset_type {
        "hg" => repo.get_bookmark(&bookmark)
            .and_then(move |cs| {
                let changeset_id_str = cs.expect("bookmark could not be found").to_string();
                let output = format_output(json_flag, changeset_id_str, "hg");
                println!("{}", output);
                future::ok(())
            })
            .boxify(),

        "bonsai" => {
            cloned!(bookmark);
            ::fetch_bonsai_changeset(bookmark.to_string().as_str(), &repo)
                .and_then(move |bonsai_cs| {
                    let changeset_id_str = bonsai_cs.get_changeset_id().to_string();
                    let output = format_output(json_flag, changeset_id_str, "bonsai");
                    println!("{}", output);
                    future::ok(())
                })
                .boxify()
        }

        _ => panic!("Unknown changeset-type supplied"),
    }
}

fn handle_set<'a>(args: &ArgMatches<'a>, _logger: Logger, repo: BlobRepo) -> BoxFuture<(), Error> {
    let bookmark_name = args.value_of("BOOKMARK_NAME").unwrap().to_string();
    let rev = args.value_of("HG_CHANGESET_ID").unwrap();
    let bookmark = Bookmark::new(bookmark_name).unwrap();

    ::fetch_bonsai_changeset(rev, &repo)
        .and_then(move |bonsai_cs| {
            let mut transaction = repo.update_bookmark_transaction();
            try_boxfuture!(transaction.force_set(&bookmark, &bonsai_cs.get_changeset_id()));
            transaction.commit().map(|_| ()).from_err().boxify()
        })
        .boxify()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_output_format() {
        let expected_answer = json!({
            "changeset_type": "hg",
            "changeset_id": "123"
        });
        assert_eq!(
            format_output(true, "123".to_string(), "hg"),
            to_string_pretty(&expected_answer).unwrap()
        );
    }

    #[test]
    fn plain_output_format() {
        assert_eq!(format_output(false, "123".to_string(), "hg"), "(HG) 123");
    }
}
