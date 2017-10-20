extern crate smart;

use smart::ata;
#[cfg(target_os = "freebsd")]
use smart::freebsd_ata;
use smart::scsi;
use smart::data::id;
use smart::data::attr;
use smart::data::health;
use smart::drivedb;
use smart::drivedb::vendor_attribute;

#[macro_use]
extern crate clap;
use clap::{App, Arg};

extern crate serde_json;
use serde_json::value::ToJson;

extern crate separator;
use separator::Separatable;
extern crate number_prefix;
use number_prefix::{decimal_prefix, binary_prefix, Standalone, Prefixed};

use std::io::Write;

fn bool_to_sup(b: bool) -> &'static str {
	match b {
		false => "not supported",
		true  => "supported",
	}
}

fn bool_to_flag(b: bool, c: char) -> char {
	if b { c } else { '-' }
}

fn print_id(id: &id::Id, dbentry: &Option<drivedb::Match>) {
	if id.incomplete { print!("WARNING: device reports information it provides is incomplete\n\n"); }

	// XXX id.is_ata is deemed redundant and is skipped
	// XXX we're skipping id.commands_supported for now as it is hardly of any interest to users

	print!("Model:    {}\n", id.model);
	match id.rpm {
		id::RPM::Unknown => (),
		id::RPM::NonRotating => print!("RPM:      N/A (SSD or other non-rotating media)\n"),
		id::RPM::RPM(i) => print!("RPM:      {}\n", i),
	};
	print!("Firmware: {}\n", id.firmware);
	print!("Serial:   {}\n", id.serial);
	// TODO: id.wwn_supported is cool, but actual WWN ID is better

	if let &Some(ref dbentry) = dbentry {
		if let Some(family) = dbentry.family {
			print!("Model family according to drive database:\n  {}\n", family);
		} else {
			print!("This drive is not in the drive database\n");
		}
		if let Some(warning) = dbentry.warning {
			print!("\n══════ WARNING ══════\n{}\n═════════════════════\n", warning);
		}
	}

	print!("\n");

	print!("Capacity: {} bytes\n", id.capacity.separated_string());
	print!("          ({}, {})\n",
		match decimal_prefix(id.capacity as f32) {
			Prefixed(p, x) => format!("{:.1} {}B", x, p),
			Standalone(x)  => format!("{} bytes", x),
		},
		match binary_prefix(id.capacity as f32) {
			Prefixed(p, x) => format!("{:.1} {}B", x, p),
			Standalone(x)  => format!("{} bytes", x),
		},
	);
	print!("Sector size (logical):  {}\n", id.sector_size_log);
	print!("Sector size (physical): {}\n", id.sector_size_phy);

	print!("\n");

	print!("ATA version:\n{}\n", id.ata_version.unwrap_or("unknown"));

	print!("\n");

	// The following guide, when printed, is exactly 80 characters
	// ... "..............................................................supported disabled\n"
	print!("Host protected area:           {}\n", id.hpa);
	print!("Advanced Power Management:     {}\n", id.apm);
	print!("Automatic Acoustic Management: {}\n", id.aam);
	print!("Read look-ahead:               {}\n", id.read_look_ahead);
	print!("Write cache:                   {}\n", id.write_cache);
	print!("Power management:              {}\n", bool_to_sup(id.power_mgmt_supported));
	print!("General purpose logging:       {}\n", bool_to_sup(id.gp_logging_supported));
	print!("Trusted computing:             {}\n", bool_to_sup(id.trusted_computing_supported));
	print!("ATA security:                  {}\n", id.security);

	print!("\n");

	print!("S.M.A.R.T.:    {}\n", id.smart);
	print!("Error logging: {}\n", bool_to_sup(id.smart_error_logging_supported));
	print!("Self-test:     {}\n", bool_to_sup(id.smart_self_test_supported));

	print!("\n");
}

// XXX only `pretty_attributes` clearly shows failing/failed attributes
fn print_attributes(values: &Vec<attr::SmartAttribute>) {
	if values.len() == 0 {
		print!("No S.M.A.R.T. attributes found.\n");
		return;
	}

	print!("S.M.A.R.T. attribute values:\n");
	print!(" ID name                     flags        value worst thresh fail raw\n");
	for val in values {
		// > The NAME … should not exceed 23 characters
		print!("{:3} {:.<24} {}{}{}{}{}{}{}    {}   {}    {} {} {}\n",
			val.id,
			val.name.as_ref().unwrap_or(&"?".to_string()),
			bool_to_flag(val.pre_fail, 'P'),
			bool_to_flag(!val.online, 'O'),
			bool_to_flag(val.performance, 'S'),
			bool_to_flag(val.error_rate, 'R'),
			bool_to_flag(val.event_count, 'C'),
			bool_to_flag(val.self_preserving, 'K'),
			if val.flags == 0 { "     ".to_string() }
				else { format!("+{:04x}", val.flags) },
			val.value.map(|v| format!("{:3}", v)).unwrap_or("---".to_string()),
			val.worst.map(|v| format!("{:3}", v)).unwrap_or("---".to_string()),
			val.thresh.map(|v| format!("{:3}", v)).unwrap_or("(?)".to_string()),
			match (val.value, val.worst, val.thresh) {
				(Some(v), _, Some(t)) if v <= t => "NOW ",
				(_, Some(w), Some(t)) if w <= t => "past",
				// either value/worst are part of the `val.row`,
				// or threshold is not available,
				// or value never was below the threshold
				_ => "-   ",
			},
			val.raw,
		);
	}
	// based on the output of 'smartctl -A -f brief' (part of 'smartctl -x')
	print!("                             │││││└─ K auto-keep\n");
	print!("                             ││││└── C event count\n");
	print!("                             │││└─── R error rate\n");
	print!("                             ││└──── S speed/performance\n");
	print!("                             │└───── O updated during off-line testing\n");
	print!("                             └────── P prefailure warning\n");
}

// this also helps maintaining serialized output (JSON) clean
fn warn(msg: String) {
	// XXX unused result
	let _ = std::io::stderr().write(msg.as_bytes());
}

// XXX macro?
fn when_smart_enabled<F>(status: &id::Ternary, action_name: &str, mut action: F) where F: FnMut() -> () {
	match *status {
		id::Ternary::Unsupported => warn(format!("S.M.A.R.T. is not supported, cannot show {}\n", action_name)),
		id::Ternary::Disabled => warn(format!("S.M.A.R.T. is disabled, cannot show {}\n", action_name)),
		id::Ternary::Enabled => action(),
	}
}

#[inline]
#[cfg(target_os = "linux")]
fn types() -> [&'static str; 2] { ["ata", "sat"] }
#[inline]
#[cfg(target_os = "freebsd")]
fn types() -> [&'static str; 1] { ["ata"] }

fn main() {
	let args = App::new("smart-rs")
		.about("yet another S.M.A.R.T. querying tool")
		.version(crate_version!())
		.arg(Arg::with_name("health")
			.short("H") // smartctl-like
			.long("health") // smartctl-like
			.help("Prints the health status of the device")
		)
		.arg(Arg::with_name("info")
			.short("i") // smartctl-like
			.long("info") // smartctl-like
			.help("Prints a basic information about the device")
		)
		.arg(Arg::with_name("attrs")
			.short("A") // smartctl-like
			.long("attributes") // smartctl-like
			.long("attrs")
			.help("Prints a list of S.M.A.R.T. attributes")
		)
		.arg(Arg::with_name("all")
			.short("a") // smartctl-like
			.long("all") // smartctl-like
			.help("equivalent to --info --health --attrs")
		)
		.arg(Arg::with_name("drivedb")
			.short("B") // smartctl-like
			.long("drivedb") // smartctl-like
			.takes_value(true)
			.value_name("FILE")
			.help("path to drivedb file") // unlike smartctl, does not support '+FILE'
		)
		.arg(Arg::with_name("vendorattribute")
			.multiple(true)
			.short("v") // smartctl-like
			.long("vendorattribute") // smartctl-like
			.takes_value(true)
			.value_name("id,format[:byteorder][,name]")
			.help("set display option for vendor attribute 'id'")
		)
		.arg(Arg::with_name("json")
			.long("json")
			.help("Export data in JSON")
		)
		.arg(Arg::with_name("type")
			.short("d") // smartctl-like
			.long("device") // smartctl-like
			.takes_value(true)
			.possible_values(&types())
			.help("device type")
		)
		.arg(Arg::with_name("device")
			.help("Device to query")
			.required(true)
			.index(1)
		)
		.get_matches();

	let (exec, task): (
		fn(&str, smart::ata::Command, u8, u8, u8) -> Result<[u8; 512], std::io::Error>,
		fn(&str, smart::ata::Command, u8, u8, u8, u8, u8, u8) -> Result<[u8; 7], std::io::Error>
	) = match args.value_of("device") {
		Some("ata") if cfg!(target_os = "linux") => (
			ata::ata_exec,
			ata::ata_task,
		),
		_ if cfg!(target_os = "linux") => (
			scsi::ata_pass_through_16_exec,
			scsi::ata_pass_through_16_task,
		),
		/* FIXME freebsd_ata is import conditionally
		Some("ata") if cfg!(target_os = "freebsd") => (
			freebsd_ata::ata_exec,
			freebsd_ata::ata_task,
		),
		*/
		_ => unreachable!(),
	};

	let file = args.value_of("device").unwrap();

	let drivedb = match args.value_of("drivedb") {
		Some(file) => drivedb::load(file),
		None =>
			drivedb::load("/var/lib/smartmontools/drivedb/drivedb.h").or(
				drivedb::load("/usr/share/smartmontools/drivedb.h")
			)
	};
	let drivedb = match drivedb {
		Ok(x) => Some(x),
		Err(e) => {
			warn(format!("Cannot open drivedb file: {}\n", e));
			None
		},
	};

	let user_attributes = args.values_of("vendorattribute")
		.map(|attrs| attrs.collect())
		.unwrap_or(vec![])
		.into_iter()
		.map(|attr| vendor_attribute::parse(attr).ok()) // TODO Err(_)
		.filter(|x| x.is_some())
		.map(|x| x.unwrap())
		.collect();

	let print_info  = args.is_present("info") || args.is_present("all");
	let print_attrs = args.is_present("attrs") || args.is_present("all");
	let print_health = args.is_present("health") || args.is_present("all");

	let use_json = args.is_present("json");
	let mut json_map = serde_json::Map::new();

	if print_info || print_attrs || print_health {
		let data = exec(&file, ata::Command::Identify, 1, 0, 1).unwrap();
		let id = id::parse_id(&data);

		let dbentry = drivedb.as_ref().map(|drivedb| drivedb::match_entry(
			&id,
			&drivedb,
			user_attributes,
		));

		if print_info {
			if use_json {
				let mut info = id.to_json().unwrap();

				if let Some(ref dbentry) = dbentry {
					if let Some(family) = dbentry.family {
						info.as_object_mut().unwrap().insert("family".to_string(), family.to_json().unwrap());
					}
					if let Some(warning) = dbentry.warning {
						info.as_object_mut().unwrap().insert("warning".to_string(), warning.to_json().unwrap());
					}
				}

				json_map.insert("info".to_string(), info);
			} else {
				print_id(&id, &dbentry);
			}
		}

		if print_health {
			when_smart_enabled(&id.smart, "health status", || {
				let data = task(&file,
					ata::Command::SMART, ata::SMARTFeature::ReturnStatus as u8,
					0, 0, 0x4f, 0xc2, 0,
				).unwrap();
				let status = health::parse_smart_status(&data);

				if use_json {
					json_map.insert("health".to_string(), status.to_json().unwrap());
				} else {
					print!("S.M.A.R.T. health status: {}\n", match status {
						Some(true) => "good",
						Some(false) => "BAD",
						None => "(unknown)",
					});
				}
			});
		}

		if print_attrs {
			when_smart_enabled(&id.smart, "attributes", || {
				let data = exec(&file, ata::Command::SMART, 0, ata::SMARTFeature::ReadValues as u8, 1).unwrap();
				let thresh = exec(&file, ata::Command::SMART, 0, ata::SMARTFeature::ReadThresholds as u8, 1).unwrap();

				let values = attr::parse_smart_values(&data, &thresh, &dbentry);

				if use_json {
					json_map.insert("attributes".to_string(), values.to_json().unwrap());
				} else {
					print_attributes(&values);
				}
			});
		}

		if use_json {
			print!("{}\n", serde_json::to_string(&json_map).unwrap());
		}
	}
}