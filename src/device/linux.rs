use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::OpenOptionsExt;

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// See [parent module docs](../index.html)
#[derive(Debug)]
pub struct Device {
    pub(crate) file: File,
}

#[derive(Debug)]
pub enum Type {
    SCSI,
}

impl Device {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, io::Error> {
        Ok(Device {
            file: OpenOptions::new()
                .read(true)
                // > Under Linux, the O_NONBLOCK flag indicates that one wants to open but does not necessarily have the intention to read or write.
                // > This is typically used to open devices in order to get a file descriptor for use with ioctl(2).
                // ~ open(2)
                // this fixes access to optical drives and other ejectable media
                // (https://github.com/vthriller/hdd-rs/issues/1)
                .custom_flags(libc::O_NONBLOCK)
                .open(path)?,
        })
    }

    pub fn get_type(&self) -> Result<Type, io::Error> {
        Ok(Type::SCSI)
    }
}

/// Lists paths to devices currently presented in the system.
pub fn list_devices() -> Result<Vec<PathBuf>, io::Error> {
    /*
    Various software enumerates block devices in a variety of ways:
    - smartd: probes for /dev/hd[a-t], /dev/sd[a-z], /dev/sd[a-c][a-z], /dev/nvme[0-99]
    - lsscsi: looks for *:* in /sys/bus/scsi/devices/, skipping {host,target}*
    - sg3_utils/sg_scan: iterates over /sys/class/scsi_generic if exists, otherwise probing for /dev/sg{0..8191} or /dev/sg{a..z,aa..zz,...}
    - util-linux/lsblk: iterates over /sys/block, skipping devices with major number 1 (RAM disks) by default (see --include/--exclude), as well as devices with no known size or the size of 0 (see /sys/class/block/<X>/size)
    - udisks: queries udev for devices in a "block" subsystem
    - gnome-disk-utility: just asks udisks
    - udev: just reads a bunch of files from /sys, appending irrelevant (in our case) data from hwdb and attributes set via various rules

    This code was once written using libudev, but it was dropped for a number of reason:
    - it's an extra dependency
    - it is much harder to make static builds for x86_64-unknown-linux-musl
    - it might not work on exotic systems that run mdev or rely solely on devtmpfs
    - data provided by libudev can be easily read from /sys
    - the data that libudev does not provide (e.g. `device/generic` symlink target for SCSI block devices), well, needs to be read from /sys anyways, so in a long run it's not, like, super-convenient to use this library
    */

    let mut devices = vec![];
    let mut skip_generics = HashSet::new();

    // XXX do not return Err() if /sys/class/block does not exist but /sys/class/scsi_generic does, or vice versa

    // N.B. log entries are indented relative to each other

    info!("inspecting /sys/class/block");
    for d in fs::read_dir("/sys/class/block")? {
        let d = if let Ok(d) = d { d } else { continue };

        // XXX this assumes that dir name equals to whatever `DEVNAME` is set to in the uevent file
        // (and that `DEVNAME` is even present there)
        let name = d.file_name();
        let path = if let Ok(path) = d.path().canonicalize() {
            path
        } else {
            debug!(
                "  {:?}: unable to read canonical device path, skipping",
                name
            );
            continue;
        };
        debug!("  {:?} → {:?}", name, path);

        // skip devices like /dev/{loop,ram,zram,md,fd}*
        if path.starts_with("/sys/devices/virtual/") {
            debug!("    virtual device, skipping");
            continue;
        }
        // Path.starts_with only works with whole path components so it can't match …/floppy.0
        // hence .to_str()
        if path
            .as_path()
            .to_str()
            .unwrap()
            .starts_with("/sys/devices/platform/floppy")
        {
            debug!("    floppy device, skipping");
            continue;
        }
        if name.to_str().unwrap().starts_with('v') {
            // probably /dev/vdX, check whether it is a virtio-blk device
            // N.B. we do NOT skip virtio_scsi devices due to LUN passthrough
            if let Ok(driver) = path.join("device/driver").read_link() {
                if driver.file_name() == Some(::std::ffi::OsStr::new("virtio_blk")) {
                    debug!("    virtio_blk device, skipping");
                    continue;
                }
            }
            // there are other ways to identify virtio devices;
            // one of them relies on PCI vendor id (`device/vendor` should read `0x1af4`, Red Hat, Inc.)
            // and device id (`../../../device` → 0x1001)
        }

        // $ grep -q '^DEVTYPE=disk$' /sys/class/block/sda/uevent
        if let Ok(uevent) = File::open(path.join("uevent")) {
            let mut is_disk = false;

            let buf = BufReader::new(uevent);
            for line in buf.lines() {
                match &line {
                    Ok(s) if s.as_str() == "DEVTYPE=disk" => {
                        debug!("    {}", s);
                        is_disk = true;
                        break;
                    }
                    Ok(s) if s.starts_with("DEVTYPE=") => {
                        debug!("    {}", s);
                        is_disk = false; // see first match arm
                        break;
                    }
                    Ok(_) => (), // keep reading
                    Err(e) => {
                        debug!("    problem reading uevent file: {}", e);
                        break;
                    }
                }
            }

            if !is_disk {
                debug!("    undisclosed block device type, or device is not a disk, skipping");
                continue;
            }
        } else {
            debug!("    unable to determine device type, skipping");
            continue;
        }

        devices.push(name);

        // e.g. `readlink /sys/class/block/sda/device/generic` → `scsi_generic/sg0`
        if let Ok(generic_path) = path.join("device/generic").read_link() {
            if let Some(generic_name) = generic_path.file_name() {
                debug!(
                    "    found corresponding scsi_generic device {:?}",
                    generic_name
                );
                skip_generics.insert(generic_name.to_os_string());
            }
        }
    }

    /*
    Some drivers (e.g. aacraid) also provide generic SCSI devices for disks behind hardware RAIDs;
    these devices can be used to query SMART or SCSI logs from disks that are not represented with corresponding block devices
    */

    info!("inspecting /sys/class/scsi_generic");
    for d in fs::read_dir("/sys/class/scsi_generic")? {
        let d = if let Ok(d) = d { d } else { continue };

        let name = d.file_name();
        debug!("  {:?}", name);

        if !skip_generics.contains(&name) {
            devices.push(name);
        } else {
            debug!("    already covered by corresponding block device, skipping");
        }
    }

    Ok(devices
        .into_iter()
        .map(|dev| PathBuf::from(format!("/dev/{}", dev.into_string().unwrap())))
        .collect())
}
