//! Systemd service actions

use std::{
    env,
    fs::{self, File},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use itertools::Itertools;
use rand::Rng;

use crate::{
    cl::HardeningOptions,
    systemd::{options::OptionWithValue, END_OPTION_OUTPUT_SNIPPET, START_OPTION_OUTPUT_SNIPPET},
};

pub(crate) struct Service {
    name: String,
    arg: Option<String>,
}

const PROFILING_FRAGMENT_NAME: &str = "profile";
const HARDENING_FRAGMENT_NAME: &str = "harden";
/// Command line prefix for `ExecStartXxx`= that bypasses all hardening options
/// See <https://www.freedesktop.org/software/systemd/man/255/systemd.service.html#Command%20lines>
const PRIVILEGED_PREFIX: &str = "+";

impl Service {
    pub(crate) fn new(unit: &str) -> Self {
        if let Some((name, arg)) = unit.split_once('@') {
            Self {
                name: name.to_owned(),
                arg: Some(arg.to_owned()),
            }
        } else {
            Self {
                name: unit.to_owned(),
                arg: None,
            }
        }
    }

    fn unit_name(&self) -> String {
        format!(
            "{}{}.service",
            &self.name,
            if let Some(arg) = self.arg.as_ref() {
                format!("@{arg}")
            } else {
                String::new()
            }
        )
    }

    pub(crate) fn add_profile_fragment(
        &self,
        hardening_opts: &HardeningOptions,
    ) -> anyhow::Result<()> {
        // Check first if our fragment does not yet exist
        let fragment_path = self.fragment_path(PROFILING_FRAGMENT_NAME, false);
        anyhow::ensure!(
            !fragment_path.is_file(),
            "Fragment config already exists at {fragment_path:?}"
        );
        let harden_fragment_path = self.fragment_path(HARDENING_FRAGMENT_NAME, true);
        anyhow::ensure!(
            !harden_fragment_path.is_file(),
            "Hardening config already exists at {harden_fragment_path:?} and may conflict with profiling"
        );

        let config_paths_bufs = self.config_paths()?;
        let config_paths = config_paths_bufs
            .iter()
            .map(PathBuf::as_path)
            .collect::<Vec<_>>();
        log::info!("Located unit config file(s): {config_paths:?}");

        // Write new fragment
        #[expect(clippy::unwrap_used)] // fragment_path guarantees by construction we have a parent
        fs::create_dir_all(fragment_path.parent().unwrap())?;
        let mut fragment_file = BufWriter::new(File::create(&fragment_path)?);
        writeln!(
            fragment_file,
            "# This file has been autogenerated by {}",
            env!("CARGO_PKG_NAME")
        )?;
        writeln!(fragment_file, "[Service]")?;
        // writeln!(fragment_file, "AmbientCapabilities=CAP_SYS_PTRACE")?;
        // needed because strace becomes the main process
        writeln!(fragment_file, "NotifyAccess=all")?;
        writeln!(fragment_file, "Environment=RUST_BACKTRACE=1")?;
        if !Self::config_vals("SystemCallFilter", &config_paths)?.is_empty() {
            // Allow ptracing, only if a syscall filter is already in place, otherwise it becomes a whitelist
            writeln!(fragment_file, "SystemCallFilter=@debug")?;
        }
        // strace may slow down enough to risk reaching some service timeouts
        writeln!(fragment_file, "TimeoutStartSec=infinity")?;
        writeln!(fragment_file, "KillMode=control-group")?;
        writeln!(fragment_file, "StandardOutput=journal")?;

        // Profile data dir
        let mut rng = rand::thread_rng();
        let profile_data_dir = PathBuf::from(format!(
            "/run/{}-profile-data_{:08x}",
            env!("CARGO_PKG_NAME"),
            rng.gen::<u32>()
        ));
        #[expect(clippy::unwrap_used)]
        writeln!(
            fragment_file,
            "RuntimeDirectory={}",
            profile_data_dir.file_name().unwrap().to_str().unwrap()
        )?;

        let shh_bin = env::current_exe()?
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Unable to decode current executable path"))?
            .to_owned();

        // Wrap ExecStartXxx directives
        let mut exec_start_idx = 1;
        let mut profile_data_paths = Vec::new();
        for exec_start_opt in ["ExecStartPre", "ExecStart", "ExecStartPost"] {
            let exec_start_cmds = Self::config_vals(exec_start_opt, &config_paths)?;
            if !exec_start_cmds.is_empty() {
                writeln!(fragment_file, "{exec_start_opt}=")?;
            }
            for cmd in exec_start_cmds {
                if cmd.starts_with(PRIVILEGED_PREFIX) {
                    // TODO handle other special prefixes?
                    // Write command unchanged
                    writeln!(fragment_file, "{exec_start_opt}={cmd}")?;
                } else {
                    let profile_data_path = profile_data_dir.join(format!("{exec_start_idx:03}"));
                    exec_start_idx += 1;
                    #[expect(clippy::unwrap_used)]
                    writeln!(
                        fragment_file,
                        "{}={} run {} -p {} -- {}",
                        exec_start_opt,
                        shh_bin,
                        hardening_opts.to_cmdline(),
                        profile_data_path.to_str().unwrap(),
                        cmd
                    )?;
                    profile_data_paths.push(profile_data_path);
                }
            }
        }

        // Add invocation that merges previous profiles
        #[expect(clippy::unwrap_used)]
        writeln!(
            fragment_file,
            "ExecStopPost={} merge-profile-data {} {}",
            shh_bin,
            hardening_opts.to_cmdline(),
            profile_data_paths
                .iter()
                .map(|p| p.to_str().unwrap())
                .join(" ")
        )?;

        log::info!("Config fragment written in {fragment_path:?}");
        Ok(())
    }

    pub(crate) fn remove_profile_fragment(&self) -> anyhow::Result<()> {
        let fragment_path = self.fragment_path(PROFILING_FRAGMENT_NAME, false);
        fs::remove_file(&fragment_path)?;
        log::info!("{fragment_path:?} removed");
        // let mut parent_dir = fragment_path;
        // while let Some(parent_dir) = parent_dir.parent() {
        //     if fs::remove_dir(parent_dir).is_err() {
        //         // Likely directory not empty
        //         break;
        //     }
        //     log::info!("{parent_dir:?} removed");
        // }
        Ok(())
    }

    pub(crate) fn remove_hardening_fragment(&self) -> anyhow::Result<()> {
        let fragment_path = self.fragment_path(HARDENING_FRAGMENT_NAME, true);
        fs::remove_file(&fragment_path)?;
        log::info!("{fragment_path:?} removed");
        Ok(())
    }

    pub(crate) fn add_hardening_fragment(&self, opts: Vec<OptionWithValue>) -> anyhow::Result<()> {
        let fragment_path = self.fragment_path(HARDENING_FRAGMENT_NAME, true);
        #[expect(clippy::unwrap_used)]
        fs::create_dir_all(fragment_path.parent().unwrap())?;

        let mut fragment_file = BufWriter::new(File::create(&fragment_path)?);
        writeln!(
            fragment_file,
            "# This file has been autogenerated by {}",
            env!("CARGO_PKG_NAME")
        )?;
        writeln!(fragment_file, "[Service]")?;
        for opt in opts {
            writeln!(fragment_file, "{opt}")?;
        }

        log::info!("Config fragment written in {fragment_path:?}");
        Ok(())
    }

    #[expect(clippy::unused_self)]
    pub(crate) fn reload_unit_config(&self) -> anyhow::Result<()> {
        let status = Command::new("systemctl").arg("daemon-reload").status()?;
        if !status.success() {
            anyhow::bail!("systemctl failed: {status}");
        }
        Ok(())
    }

    pub(crate) fn action(&self, verb: &str, block: bool) -> anyhow::Result<()> {
        let unit_name = self.unit_name();
        log::info!("{} {}", verb, unit_name);
        let mut cmd = vec![verb];
        if !block {
            cmd.push("--no-block");
        }
        cmd.push(&unit_name);
        let status = Command::new("systemctl").args(cmd).status()?;
        if !status.success() {
            anyhow::bail!("systemctl failed: {status}");
        }
        Ok(())
    }

    pub(crate) fn profiling_result(&self) -> anyhow::Result<Vec<OptionWithValue>> {
        // Start journalctl process
        let mut child = Command::new("journalctl")
            .args([
                "-r",
                "-o",
                "cat",
                "--output-fields=MESSAGE",
                "--no-tail",
                "-u",
                &self.unit_name(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env("LANG", "C")
            .spawn()?;

        // Parse its output
        #[expect(clippy::unwrap_used)]
        let reader = BufReader::new(child.stdout.take().unwrap());
        let snippet_lines: Vec<_> = reader
            .lines()
            // Stream lines but bubble up errors
            .skip_while(|r| {
                r.as_ref()
                    .map(|l| l != END_OPTION_OUTPUT_SNIPPET)
                    .unwrap_or(false)
            })
            .take_while_inclusive(|r| {
                r.as_ref()
                    .map(|l| l != START_OPTION_OUTPUT_SNIPPET)
                    .unwrap_or(true)
            })
            .collect::<Result<_, _>>()?;
        if (snippet_lines.len() < 2)
            || (snippet_lines
                .last()
                .ok_or_else(|| anyhow::anyhow!("Unable to get profiling result lines"))?
                != START_OPTION_OUTPUT_SNIPPET)
        {
            anyhow::bail!("Unable to get profiling result snippet");
        }
        // The output with '-r' flag is in reverse chronological order
        // (to get the end as fast as possible), so reverse it, after we have
        // removed marker lines
        let opts = snippet_lines[1..snippet_lines.len() - 1]
            .iter()
            .rev()
            .map(|l| l.parse::<OptionWithValue>())
            .collect::<anyhow::Result<_>>()?;

        // Stop journalctl
        child.kill()?;
        child.wait()?;

        Ok(opts)
    }

    fn config_vals(key: &str, config_paths: &[&Path]) -> anyhow::Result<Vec<String>> {
        // Note: we could use 'systemctl show -p xxx' but its output is different from config
        // files, and we would need to interpret it anyway
        let mut vals = Vec::new();
        for config_path in config_paths {
            let config_file = BufReader::new(File::open(config_path)?);
            let prefix = format!("{key}=");
            let mut file_vals = vec![];
            let mut lines = config_file.lines();
            while let Some(line) = lines.next() {
                let line = line?;
                if line.starts_with(&prefix) {
                    let val = if line.ends_with('\\') {
                        let mut val = line
                            .split_once('=')
                            .ok_or_else(|| anyhow::anyhow!("Unable to parse service option line"))?
                            .1
                            .trim()
                            .to_owned();
                        // Remove trailing '\'
                        val.pop();
                        // Append next lines
                        loop {
                            let next_line = lines
                                .next()
                                .ok_or_else(|| anyhow::anyhow!("Unexpected end of file"))??;
                            val = format!("{} {}", val, next_line.trim_start());
                            if next_line.ends_with('\\') {
                                // Remove trailing '\'
                                val.pop();
                            } else {
                                break;
                            }
                        }
                        val
                    } else {
                        line.split_once('=')
                            .ok_or_else(|| anyhow::anyhow!("Unable to parse service option line"))?
                            .1
                            .trim()
                            .to_owned()
                    };
                    file_vals.push(val);
                }
            }
            while let Some(clear_idx) = file_vals.iter().position(String::is_empty) {
                file_vals = file_vals[clear_idx + 1..].to_vec();
                vals.clear();
            }
            vals.extend(file_vals);
        }
        Ok(vals)
    }

    fn config_paths(&self) -> anyhow::Result<Vec<PathBuf>> {
        let output = Command::new("systemctl")
            .args(["status", "-n", "0", &self.unit_name()])
            .env("LANG", "C")
            .output()?;
        let mut paths = Vec::new();
        let mut drop_in_dir = None;
        for line in output.stdout.lines() {
            let line = line?;
            let line = line.trim_start();
            if line.starts_with("Loaded:") {
                // Main unit file
                anyhow::ensure!(paths.is_empty());
                let path = line
                    .split_once('(')
                    .ok_or_else(|| anyhow::anyhow!("Failed to locate main unit file"))?
                    .1
                    .split_once(';')
                    .ok_or_else(|| anyhow::anyhow!("Failed to locate main unit file"))?
                    .0;
                paths.push(PathBuf::from(path));
            } else if line.starts_with("Drop-In:") {
                // Drop in base dir
                anyhow::ensure!(paths.len() == 1);
                anyhow::ensure!(drop_in_dir.is_none());
                let dir = line
                    .split_once(':')
                    .ok_or_else(|| anyhow::anyhow!("Failed to locate unit config fragment dir"))?
                    .1
                    .trim_start();
                drop_in_dir = Some(PathBuf::from(dir));
            } else if let Some(dir) = drop_in_dir.as_ref() {
                if line.contains(':') {
                    // Not a path, next key: val line
                    break;
                } else if line.starts_with('/') {
                    // New base dir
                    drop_in_dir = Some(PathBuf::from(line));
                } else {
                    for filename in line.trim().chars().skip(2).collect::<String>().split(", ") {
                        let path = dir.join(filename);
                        paths.push(path);
                    }
                }
            }
        }
        Ok(paths)
    }

    fn fragment_path(&self, name: &str, persistent: bool) -> PathBuf {
        [
            if persistent { "/etc" } else { "/run" },
            "systemd/system/",
            &format!(
                "{}{}.service.d",
                self.name,
                if self.arg.is_some() { "@" } else { "" }
            ),
            &format!("zz_{}-{}.conf", env!("CARGO_PKG_NAME"), name),
        ]
        .iter()
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_vals() {
        let _ = simple_logger::SimpleLogger::new().init();

        let mut cfg_file1 = tempfile::NamedTempFile::new().unwrap();
        let mut cfg_file2 = tempfile::NamedTempFile::new().unwrap();
        let mut cfg_file3 = tempfile::NamedTempFile::new().unwrap();

        writeln!(cfg_file1, "blah=a").unwrap();
        writeln!(cfg_file1, "blah=b").unwrap();
        writeln!(cfg_file2, "blah=").unwrap();
        writeln!(cfg_file2, "blah=c").unwrap();
        writeln!(cfg_file2, "blih=e").unwrap();
        writeln!(cfg_file2, "bloh=f").unwrap();
        writeln!(cfg_file3, "blah=d").unwrap();

        assert_eq!(
            Service::config_vals(
                "blah",
                &[cfg_file1.path(), cfg_file2.path(), cfg_file3.path()]
            )
            .unwrap(),
            vec!["c", "d"]
        );
    }

    #[test]
    fn test_config_val_multiline() {
        let _ = simple_logger::SimpleLogger::new().init();

        let mut cfg_file = tempfile::NamedTempFile::new().unwrap();

        writeln!(
            cfg_file,
            r#"ExecStartPre=/bin/sh -c "[ ! -e /usr/bin/galera_recovery ] && VAR= || \
 VAR=`cd /usr/bin/..; /usr/bin/galera_recovery`; [ $? -eq 0 ] \
 && systemctl set-environment _WSREP_START_POSITION=$VAR || exit 1""#
        )
        .unwrap();

        assert_eq!(
            Service::config_vals("ExecStartPre", &[cfg_file.path()]).unwrap(),
            vec![
                r#"/bin/sh -c "[ ! -e /usr/bin/galera_recovery ] && VAR= ||  VAR=`cd /usr/bin/..; /usr/bin/galera_recovery`; [ $? -eq 0 ]  && systemctl set-environment _WSREP_START_POSITION=$VAR || exit 1""#
            ]
        );
    }
}