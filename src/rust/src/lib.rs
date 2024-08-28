//! Rust library for CCExtractor
//!
//! Currently we are in the process of porting the 708 decoder to rust. See [decoder]

// Allow C naming style
#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

/// CCExtractor C bindings generated by bindgen
#[allow(clippy::all)]
pub mod bindings {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

pub mod args;
pub mod common;
pub mod decoder;
#[cfg(feature = "hardsubx_ocr")]
pub mod hardsubx;
pub mod libccxr_exports;
pub mod parser;
pub mod utils;

#[cfg(windows)]
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::{io::Write, os::raw::c_char, os::raw::c_int};

use args::Args;
use bindings::*;
use clap::{error::ErrorKind, Parser};
use common::{CType, CType2, FromRust};
use decoder::Dtvcc;
use lib_ccxr::{common::Options, teletext::TeletextConfig, util::log::ExitCause};
use parser::OptionsExt;
use utils::is_true;

use env_logger::{builder, Target};
use log::{warn, LevelFilter};
use std::ffi::CStr;

#[cfg(test)]
static mut cb_708: c_int = 0;
#[cfg(test)]
static mut cb_field1: c_int = 0;
#[cfg(test)]
static mut cb_field2: c_int = 0;

#[cfg(not(test))]
extern "C" {
    static mut cb_708: c_int;
    static mut cb_field1: c_int;
    static mut cb_field2: c_int;
}

#[allow(dead_code)]
extern "C" {
    static mut usercolor_rgb: [c_int; 8];
    static mut FILEBUFFERSIZE: c_int;
    static mut MPEG_CLOCK_FREQ: c_int;
    static mut tlt_config: ccx_s_teletext_config;
    static mut ccx_options: ccx_s_options;
    static mut capitalization_list: word_list;
    static mut profane: word_list;
}

/// Initialize env logger with custom format, using stdout as target
#[no_mangle]
pub extern "C" fn ccxr_init_logger() {
    builder()
        .format(|buf, record| writeln!(buf, "[CEA-708] {}", record.args()))
        .filter_level(LevelFilter::Debug)
        .target(Target::Stdout)
        .init();
}

/// Process cc_data
///
/// # Safety
/// dec_ctx should not be a null pointer
/// data should point to cc_data of length cc_count
#[no_mangle]
extern "C" fn ccxr_process_cc_data(
    dec_ctx: *mut lib_cc_decode,
    data: *const ::std::os::raw::c_uchar,
    cc_count: c_int,
) -> c_int {
    let mut ret = -1;
    let mut cc_data: Vec<u8> = (0..cc_count * 3)
        .map(|x| unsafe { *data.add(x as usize) })
        .collect();
    let dec_ctx = unsafe { &mut *dec_ctx };
    let dtvcc_ctx = unsafe { &mut *dec_ctx.dtvcc };
    let mut dtvcc = Dtvcc::new(dtvcc_ctx);
    for cc_block in cc_data.chunks_exact_mut(3) {
        if !validate_cc_pair(cc_block) {
            continue;
        }
        let success = do_cb(dec_ctx, &mut dtvcc, cc_block);
        if success {
            ret = 0;
        }
    }
    ret
}

/// Returns `true` if cc_block pair is valid
///
/// For CEA-708 data, only cc_valid is checked
/// For CEA-608 data, parity is also checked
pub fn validate_cc_pair(cc_block: &mut [u8]) -> bool {
    let cc_valid = (cc_block[0] & 4) >> 2;
    let cc_type = cc_block[0] & 3;
    if cc_valid == 0 {
        return false;
    }
    if cc_type == 0 || cc_type == 1 {
        // For CEA-608 data we verify parity.
        if verify_parity(cc_block[2]) {
            // If the second byte doesn't pass parity, ignore pair
            return false;
        }
        if verify_parity(cc_block[1]) {
            // If the first byte doesn't pass parity,
            // we replace it with a solid blank and process the pair.
            cc_block[1] = 0x7F;
        }
    }
    true
}

/// Returns `true` if data has odd parity
///
/// CC uses odd parity (i.e., # of 1's in byte is odd.)
pub fn verify_parity(data: u8) -> bool {
    if data.count_ones() & 1 == 1 {
        return true;
    }
    false
}

/// Process CC data according to its type
pub fn do_cb(ctx: &mut lib_cc_decode, dtvcc: &mut Dtvcc, cc_block: &[u8]) -> bool {
    let cc_valid = (cc_block[0] & 4) >> 2;
    let cc_type = cc_block[0] & 3;
    let mut timeok = true;

    if ctx.write_format != ccx_output_format::CCX_OF_DVDRAW
        && ctx.write_format != ccx_output_format::CCX_OF_RAW
        && (cc_block[0] == 0xFA || cc_block[0] == 0xFC || cc_block[0] == 0xFD)
        && (cc_block[1] & 0x7F) == 0
        && (cc_block[2] & 0x7F) == 0
    {
        return true;
    }

    if cc_valid == 1 || cc_type == 3 {
        ctx.cc_stats[cc_type as usize] += 1;
        match cc_type {
            // Type 0 and 1 are for CEA-608 data. Handled by C code, do nothing
            0 | 1 => {}
            // Type 2 and 3 are for CEA-708 data.
            2 | 3 => {
                let current_time = unsafe { (*ctx.timing).get_fts(ctx.current_field as u8) };
                ctx.current_field = 3;

                // Check whether current time is within start and end bounds
                if is_true(ctx.extraction_start.set)
                    && current_time < ctx.extraction_start.time_in_ms
                {
                    timeok = false;
                }
                if is_true(ctx.extraction_end.set) && current_time > ctx.extraction_end.time_in_ms {
                    timeok = false;
                    ctx.processed_enough = 1;
                }

                if timeok && ctx.write_format != ccx_output_format::CCX_OF_RAW {
                    dtvcc.process_cc_data(cc_valid, cc_type, cc_block[1], cc_block[2]);
                }
                unsafe { cb_708 += 1 }
            }
            _ => warn!("Invalid cc_type"),
        }
    }
    true
}

#[cfg(windows)]
#[no_mangle]
extern "C" fn ccxr_close_handle(handle: RawHandle) {
    use std::fs::File;

    if handle.is_null() {
        return;
    }
    unsafe {
        // File will close automatically (due to Drop) once it goes out of scope
        let _file = File::from_raw_handle(handle);
    }
}

extern "C" {
    fn version(location: *const c_char);
    #[allow(dead_code)]
    fn set_binary_mode();
}

/// # Safety
/// Safe if argv is a valid pointer
///
/// Parse parameters from argv and argc
#[no_mangle]
pub unsafe extern "C" fn ccxr_parse_parameters(argc: c_int, argv: *mut *mut c_char) -> c_int {
    // Convert argv to Vec<String> and pass it to parse_parameters
    let args = std::slice::from_raw_parts(argv, argc as usize)
        .iter()
        .map(|&arg| {
            CStr::from_ptr(arg)
                .to_str()
                .expect("Invalid UTF-8 sequence in argument")
                .to_owned()
        })
        .collect::<Vec<String>>();

    if args.len() <= 1 {
        return ExitCause::NoInputFiles.exit_code();
    }

    let args: Args = match Args::try_parse_from(args) {
        Ok(args) => args,
        Err(e) => {
            // Not all errors are actual errors, some are just help or version
            // So handle them accordingly
            match e.kind() {
                ErrorKind::DisplayHelp => {
                    // Print the help string
                    println!("{}", e);
                    return ExitCause::WithHelp.exit_code();
                }
                ErrorKind::DisplayVersion => {
                    version(*argv);
                    return ExitCause::WithHelp.exit_code();
                }
                ErrorKind::UnknownArgument => {
                    println!("Unknown Argument");
                    println!("{}", e);
                    return ExitCause::MalformedParameter.exit_code();
                }
                _ => {
                    println!("{}", e);
                    return ExitCause::Failure.exit_code();
                }
            }
        }
    };

    let mut _capitalization_list: Vec<String> = Vec::new();
    let mut _profane: Vec<String> = Vec::new();

    let mut opt = Options::default();
    let mut _tlt_config = TeletextConfig::default();

    opt.parse_parameters(
        &args,
        &mut _tlt_config,
        &mut _capitalization_list,
        &mut _profane,
    );
    tlt_config = _tlt_config.to_ctype(&opt);

    // Convert the rust struct (CcxOptions) to C struct (ccx_s_options), so that it can be used by the C code
    ccx_options.copy_from_rust(opt);

    if !_capitalization_list.is_empty() {
        capitalization_list = _capitalization_list.to_ctype();
    }
    if !_profane.is_empty() {
        profane = _profane.to_ctype();
    }

    ExitCause::Ok.exit_code()
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_verify_parity() {
        // Odd parity
        assert!(verify_parity(0b1010001));

        // Even parity
        assert!(!verify_parity(0b1000001));
    }

    #[test]
    fn test_validate_cc_pair() {
        // Valid CEA-708 data
        let mut cc_block = [0x97, 0x1F, 0x3C];
        assert!(validate_cc_pair(&mut cc_block));

        // Invalid CEA-708 data
        let mut cc_block = [0x93, 0x1F, 0x3C];
        assert!(!validate_cc_pair(&mut cc_block));

        // Valid CEA-608 data
        let mut cc_block = [0x15, 0x2F, 0x7D];
        assert!(validate_cc_pair(&mut cc_block));
        // Check for replaced bit when 1st byte doesn't pass parity
        assert_eq!(cc_block[1], 0x7F);

        // Invalid CEA-608 data
        let mut cc_block = [0x15, 0x2F, 0x5E];
        assert!(!validate_cc_pair(&mut cc_block));
    }

    #[test]
    fn test_do_cb() {
        let mut dtvcc_ctx = utils::get_zero_allocated_obj::<dtvcc_ctx>();
        let mut dtvcc = Dtvcc::new(&mut dtvcc_ctx);

        let mut decoder_ctx = lib_cc_decode::default();
        let cc_block = [0x97, 0x1F, 0x3C];

        assert!(do_cb(&mut decoder_ctx, &mut dtvcc, &cc_block));
        assert_eq!(decoder_ctx.current_field, 3);
        assert_eq!(decoder_ctx.cc_stats[3], 1);
        assert_eq!(decoder_ctx.processed_enough, 0);
        assert_eq!(unsafe { cb_708 }, 11);
    }
}
