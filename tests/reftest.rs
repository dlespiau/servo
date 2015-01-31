// Copyright 2013 The Servo Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#![deny(unused_imports)]
#![deny(unused_variables)]

extern crate png;
extern crate test;
extern crate regex;
extern crate url;

use std::ascii::AsciiExt;
use std::old_io;
use std::old_io::{File, Reader, Command, IoResult};
use std::old_io::process::ExitStatus;
use std::old_io::fs::PathExtensions;
use std::os;
use std::path::Path;
use std::thunk::Thunk;
use test::{AutoColor, DynTestName, DynTestFn, TestDesc, TestOpts, TestDescAndFn, ShouldFail};
use test::run_tests_console;
use regex::Regex;
use url::Url;


bitflags!(
    flags RenderMode: u32 {
        const CPU_RENDERING  = 0x00000001,
        const GPU_RENDERING  = 0x00000010,
        const LINUX_TARGET   = 0x00000100,
        const MACOS_TARGET   = 0x00001000,
        const ANDROID_TARGET = 0x00010000
    }
);


fn main() {
    let args = os::args();
    let mut parts = args.tail().split(|e| "--" == e.as_slice());

    let harness_args = parts.next().unwrap();  // .split() is never empty
    let servo_args = parts.next().unwrap_or(&[]);

    let (render_mode_string, base_path, testname) = match harness_args {
        [] | [_] => panic!("USAGE: cpu|gpu base_path [testname regex]"),
        [ref render_mode_string, ref base_path] => (render_mode_string, base_path, None),
        [ref render_mode_string, ref base_path, ref testname, ..] => (render_mode_string, base_path, Some(Regex::new(testname.as_slice()).unwrap())),
    };

    let mut render_mode = match render_mode_string.as_slice() {
        "cpu" => CPU_RENDERING,
        "gpu" => GPU_RENDERING,
        _ => panic!("First argument must specify cpu or gpu as rendering mode")
    };
    if cfg!(target_os = "linux") {
        render_mode.insert(LINUX_TARGET);
    }
    if cfg!(target_os = "macos") {
        render_mode.insert(MACOS_TARGET);
    }
    if cfg!(target_os = "android") {
        render_mode.insert(ANDROID_TARGET);
    }

    let mut all_tests = vec!();
    println!("Scanning {} for manifests\n", base_path);

    for file in io::fs::walk_dir(&Path::new(base_path.as_slice())).unwrap() {
        let maybe_extension = file.extension_str();
        match maybe_extension {
            Some(extension) => {
                if extension.to_ascii_lowercase().as_slice() == "list" && file.is_file() {
                    let tests = parse_lists(&file, servo_args, render_mode, all_tests.len());
                    println!("\t{} [{} tests]", file.display(), tests.len());
                    all_tests.extend(tests.into_iter());
                }
            }
            _ => {}
        }
    }

    let test_opts = TestOpts {
        filter: testname,
        run_ignored: false,
        logfile: None,
        run_tests: true,
        run_benchmarks: false,
        ratchet_noise_percent: None,
        ratchet_metrics: None,
        save_metrics: None,
        test_shard: None,
        nocapture: false,
        color: AutoColor,
        show_boxplot: false,
        boxplot_width: 0,
        show_all_stats: false,
    };

    match run(test_opts,
              all_tests,
              servo_args.iter().map(|x| x.clone()).collect()) {
        Ok(false) => os::set_exit_status(1), // tests failed
        Err(_) => os::set_exit_status(2),    // I/O-related failure
        _ => (),
    }
}

fn run(test_opts: TestOpts, all_tests: Vec<TestDescAndFn>,
       servo_args: Vec<String>) -> IoResult<bool> {
    // Verify that we're passing in valid servo arguments. Otherwise, servo
    // will exit before we've run any tests, and it will appear to us as if
    // all the tests are failing.
    let mut command = match Command::new(os::self_exe_path().unwrap().join("servo"))
                            .args(servo_args.as_slice()).spawn() {
        Ok(p) => p,
        Err(e) => panic!("failed to execute process: {}", e),
    };
    let stderr = command.stderr.as_mut().unwrap().read_to_string().unwrap();

    if stderr.as_slice().contains("Unrecognized") {
        println!("Servo: {}", stderr.as_slice());
        return Ok(false);
    }

    run_tests_console(&test_opts, all_tests)
}

#[derive(PartialEq)]
enum ReftestKind {
    Same,
    Different,
}

struct Reftest {
    name: String,
    kind: ReftestKind,
    files: [Path; 2],
    id: uint,
    servo_args: Vec<String>,
    render_mode: RenderMode,
    is_flaky: bool,
    experimental: bool,
    fragment_identifier: Option<String>,
}

struct TestLine<'a> {
    conditions: &'a str,
    kind: &'a str,
    file_left: &'a str,
    file_right: &'a str,
}

fn parse_lists(file: &Path, servo_args: &[String], render_mode: RenderMode, id_offset: uint) -> Vec<TestDescAndFn> {
    let mut tests = Vec::new();
    let contents = File::open_mode(file, io::Open, io::Read)
                       .and_then(|mut f| f.read_to_string())
                       .ok().expect("Could not read file");

    for line in contents.as_slice().lines() {
        // ignore comments or empty lines
        if line.starts_with("#") || line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.split(' ').filter(|p| !p.is_empty()).collect();

        let test_line = match parts.len() {
            3 => TestLine {
                conditions: "",
                kind: parts[0],
                file_left: parts[1],
                file_right: parts[2],
            },
            4 => TestLine {
                conditions: parts[0],
                kind: parts[1],
                file_left: parts[2],
                file_right: parts[3],
            },
            _ => panic!("reftest line: '{}' doesn't match '[CONDITIONS] KIND LEFT RIGHT'", line),
        };

        let kind = match test_line.kind {
            "==" => ReftestKind::Same,
            "!=" => ReftestKind::Different,
            part => panic!("reftest line: '{}' has invalid kind '{}'", line, part)
        };

        // If we're running this directly, file.dir_path() might be relative.
        // (see issue #3521)
        let base = match file.dir_path().is_relative() {
            true  => os::getcwd().unwrap().join(file.dir_path()),
            false => file.dir_path()
        };

        let file_left =  base.join(test_line.file_left);
        let file_right = base.join(test_line.file_right);

        let mut conditions_list = test_line.conditions.split(',');
        let mut flakiness = RenderMode::empty();
        let mut experimental = false;
        let mut fragment_identifier = None;
        for condition in conditions_list {
            match condition {
                "flaky_cpu" => flakiness.insert(CPU_RENDERING),
                "flaky_gpu" => flakiness.insert(GPU_RENDERING),
                "flaky_linux" => flakiness.insert(LINUX_TARGET),
                "flaky_macos" => flakiness.insert(MACOS_TARGET),
                "experimental" => experimental = true,
                _ => (),
            }
            if condition.starts_with("fragment=") {
                fragment_identifier = Some(condition.slice_from("fragment=".len()).to_string());
            }
        }

        let reftest = Reftest {
            name: format!("{} {} {}", test_line.file_left, test_line.kind, test_line.file_right),
            kind: kind,
            files: [file_left, file_right],
            id: id_offset + tests.len(),
            render_mode: render_mode,
            servo_args: servo_args.iter().map(|x| x.clone()).collect(),
            is_flaky: render_mode.intersects(flakiness),
            experimental: experimental,
            fragment_identifier: fragment_identifier,
        };

        tests.push(make_test(reftest));
    }
    tests
}

fn make_test(reftest: Reftest) -> TestDescAndFn {
    let name = reftest.name.clone();
    TestDescAndFn {
        desc: TestDesc {
            name: DynTestName(name),
            ignore: false,
            should_fail: ShouldFail::No,
        },
        testfn: DynTestFn(Thunk::new(move || {
            check_reftest(reftest);
        })),
    }
}

fn capture(reftest: &Reftest, side: uint) -> (u32, u32, Vec<u8>) {
    let png_filename = format!("/tmp/servo-reftest-{:06}-{}.png", reftest.id, side);
    let mut command = Command::new(os::self_exe_path().unwrap().join("servo"));
    command
        .args(reftest.servo_args.as_slice())
        // Allows pixel perfect rendering of Ahem font for reftests.
        .arg("-Z")
        .arg("disable-text-aa")
        .args(["-f", "-o"].as_slice())
        .arg(png_filename.as_slice())
        .arg({
            let mut url = Url::from_file_path(&reftest.files[side]).unwrap();
            url.fragment = reftest.fragment_identifier.clone();
            url.to_string()
        });
    // CPU rendering is the default
    if reftest.render_mode.contains(CPU_RENDERING) {
        command.arg("-c");
    }
    if reftest.render_mode.contains(GPU_RENDERING) {
        command.arg("-g");
    }
    if reftest.experimental {
        command.arg("--experimental");
    }
    let retval = match command.status() {
        Ok(status) => status,
        Err(e) => panic!("failed to execute process: {}", e),
    };
    assert_eq!(retval, ExitStatus(0));

    let path = png_filename.parse::<Path>().unwrap();
    let image = png::load_png(&path).unwrap();
    let rgba8_bytes = match image.pixels {
        png::PixelsByColorType::RGBA8(pixels) => pixels,
        _ => panic!(),
    };
    (image.width, image.height, rgba8_bytes)
}

fn check_reftest(reftest: Reftest) {
    let (left_width, left_height, left_bytes) = capture(&reftest, 0);
    let (right_width, right_height, right_bytes) = capture(&reftest, 1);

    assert_eq!(left_width, right_width);
    assert_eq!(left_height, right_height);

    let left_all_white = left_bytes.iter().all(|&p| p == 255);
    let right_all_white = right_bytes.iter().all(|&p| p == 255);

    if left_all_white && right_all_white {
        panic!("Both renderings are empty")
    }

    let pixels = left_bytes.iter().zip(right_bytes.iter()).map(|(&a, &b)| {
        if a as i8 - b as i8 == 0 {
            // White for correct
            0xFF
        } else {
            // "1100" in the RGBA channel with an error for an incorrect value
            // This results in some number of C0 and FFs, which is much more
            // readable (and distinguishable) than the previous difference-wise
            // scaling but does not require reconstructing the actual RGBA pixel.
            0xC0
        }
    }).collect::<Vec<u8>>();

    if pixels.iter().any(|&a| a < 255) {
        let output_str = format!("/tmp/servo-reftest-{:06}-diff.png", reftest.id);
        let output = output_str.parse::<Path>().unwrap();

        let mut img = png::Image {
            width: left_width,
            height: left_height,
            pixels: png::PixelsByColorType::RGBA8(pixels),
        };
        let res = png::store_png(&mut img, &output);
        assert!(res.is_ok());

        match (reftest.kind, reftest.is_flaky) {
            (ReftestKind::Same, true) => println!("flaky test - rendering difference: {}", output_str),
            (ReftestKind::Same, false) => panic!("rendering difference: {}", output_str),
            (ReftestKind::Different, _) => {}   // Result was different and that's what was expected
        }
    } else {
        assert!(reftest.is_flaky || reftest.kind == ReftestKind::Same);
    }
}
