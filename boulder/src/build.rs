// SPDX-FileCopyrightText: Copyright © 2020-2024 Serpent OS Developers
//
// SPDX-License-Identifier: MPL-2.0

use std::{
    io,
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process, thread,
    time::Duration,
};

use itertools::Itertools;
use moss::runtime;
use nix::{
    sys::signal::Signal,
    unistd::{getpgrp, setpgid, Pid},
};
use stone_recipe::{
    script::{self, Breakpoint},
    Script,
};
use thiserror::Error;
use tui::Stylize;

pub mod job;
mod pgo;
mod root;
mod upstream;

use self::job::Job;
use crate::{
    architecture::BuildTarget,
    container, macros,
    package::{self, Packager},
    profile, recipe, util, Env, Macros, Paths, Recipe,
};

pub struct Builder {
    pub targets: Vec<Target>,
    pub recipe: Recipe,
    pub paths: Paths,
    pub macros: Macros,
    pub ccache: bool,
    pub env: Env,
    profile: profile::Id,
}

pub struct Target {
    pub build_target: BuildTarget,
    pub jobs: Vec<Job>,
}

impl Builder {
    pub fn new(recipe_path: &Path, env: Env, profile: profile::Id, ccache: bool) -> Result<Self, Error> {
        let recipe = Recipe::load(recipe_path)?;

        let macros = Macros::load(&env)?;

        let paths = Paths::new(&recipe, &env.cache_dir, "/mason")?;

        let build_targets = recipe.build_targets();

        if build_targets.is_empty() {
            return Err(Error::NoBuildTargets);
        }

        let targets = build_targets
            .into_iter()
            .map(|build_target| {
                let stages = pgo::stages(&recipe, build_target)
                    .map(|stages| stages.into_iter().map(Some).collect::<Vec<_>>())
                    .unwrap_or_else(|| vec![None]);

                let jobs = stages
                    .into_iter()
                    .map(|stage| Job::new(build_target, stage, &recipe, &paths, &macros, ccache))
                    .collect::<Result<Vec<_>, _>>()?;

                Ok(Target { build_target, jobs })
            })
            .collect::<Result<Vec<_>, job::Error>>()?;

        Ok(Self {
            targets,
            recipe,
            paths,
            macros,
            ccache,
            env,
            profile,
        })
    }

    pub fn extra_deps(&self) -> impl Iterator<Item = &str> {
        self.targets.iter().flat_map(|target| {
            target.jobs.iter().flat_map(|job| {
                job.phases
                    .values()
                    .flat_map(|script| script.dependencies.iter().map(String::as_str))
            })
        })
    }

    pub fn setup(&self) -> Result<(), Error> {
        root::clean(self)?;

        let rt = runtime::init();

        let profiles = profile::Manager::new(&self.env);
        let repos = profiles.repositories(&self.profile)?.clone();

        root::populate(self, repos)?;
        upstream::sync(&self.recipe, &self.paths)?;

        rt.destroy();
        // We want to ensure no threads exist before
        // cloning into container. Sometimes a deadlock
        // occurs which appears related to a race condition
        // from some thread artifacts still existing. Adding
        // this delay allows things to get cleaned up.
        // NOTE: This appears to reliably fix the problem,
        // I ran boulder 100 times w/ and w/out this delay
        // and the deadlock never occured w/ it, but w/out
        // it occured within 10 attempts.
        thread::sleep(Duration::from_millis(50));

        Ok(())
    }

    pub fn build(self) -> Result<Packager, Error> {
        container::exec(&self.paths, self.recipe.parsed.options.networking, || {
            // We're now in the container =)

            // Set ourselves into our own process group
            // and set it as fg term
            //
            // This is so we can restore this process back as
            // the fg term after using `bash` for chroot below
            // so we can reestablish SIGINT forwarding to scripts
            setpgid(Pid::from_raw(0), Pid::from_raw(0))?;
            let pgid = getpgrp();
            ::container::set_term_fg(pgid)?;

            for (i, target) in self.targets.iter().enumerate() {
                if i > 0 {
                    println!();
                }
                println!("{}", target.build_target.to_string().dim());

                for (i, job) in target.jobs.iter().enumerate() {
                    let is_pgo = job.pgo_stage.is_some();

                    // Recreate work dir for each job
                    util::recreate_dir(&job.work_dir)?;
                    // Ensure pgo dir exists
                    if is_pgo {
                        let pgo_dir = PathBuf::from(format!("{}-pgo", job.build_dir.display()));
                        util::ensure_dir_exists(&pgo_dir)?;
                    }

                    if let Some(stage) = job.pgo_stage {
                        if i > 0 {
                            println!("{}", "│".dim());
                        }
                        println!("{}", format!("│pgo-{stage}").dim());
                    }

                    for (i, (phase, script)) in job.phases.iter().enumerate() {
                        let pipes = if job.pgo_stage.is_some() {
                            "││".dim()
                        } else {
                            "│".dim()
                        };

                        if i > 0 {
                            println!("{pipes}");
                        }
                        println!("{pipes}{}", phase.styled(format!("{phase}")));

                        let build_dir = &job.build_dir;
                        let work_dir = &job.work_dir;
                        let current_dir = if work_dir.exists() { &work_dir } else { &build_dir };

                        for command in &script.commands {
                            match command {
                                script::Command::Break(breakpoint) => {
                                    let line_num = breakpoint_line(breakpoint, &self.recipe, job.target, *phase)
                                        .map(|line_num| format!(" at line {line_num}"))
                                        .unwrap_or_default();

                                    println!(
                                        "\n{}{} {}",
                                        "Breakpoint".bold(),
                                        line_num,
                                        if breakpoint.exit {
                                            "(exit)".dim()
                                        } else {
                                            "(continue)".dim()
                                        },
                                    );

                                    // Write env to $HOME/.profile
                                    std::fs::write(build_dir.join(".profile"), format_profile(script))?;

                                    let mut command = process::Command::new("/bin/bash")
                                        .arg("--login")
                                        .env_clear()
                                        .env("HOME", build_dir)
                                        .env("PATH", "/usr/bin:/usr/sbin")
                                        .env("TERM", "xterm-256color")
                                        .current_dir(current_dir)
                                        .spawn()?;

                                    command.wait()?;

                                    // Restore ourselves as fg term since bash steals it
                                    ::container::set_term_fg(pgid)?;

                                    if breakpoint.exit {
                                        return Ok(());
                                    }
                                }
                                script::Command::Content(content) => {
                                    // TODO: Proper temp file
                                    let script_path = "/tmp/script";
                                    std::fs::write(script_path, content).unwrap();

                                    let result = logged(*phase, is_pgo, "/bin/sh", |command| {
                                        command
                                            .arg(script_path)
                                            .env_clear()
                                            .env("HOME", build_dir)
                                            .env("PATH", "/usr/bin:/usr/sbin")
                                            .current_dir(current_dir)
                                    })?;

                                    if !result.success() {
                                        match result.code() {
                                            Some(code) => {
                                                return Err(ExecError::Code(code));
                                            }
                                            None => {
                                                if let Some(signal) = result
                                                    .signal()
                                                    .or_else(|| result.stopped_signal())
                                                    .and_then(|i| Signal::try_from(i).ok())
                                                {
                                                    return Err(ExecError::Signal(signal));
                                                } else {
                                                    return Err(ExecError::UnknownSignal);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            println!();

            Ok(())
        })?;

        let packager = Packager::new(self.paths, self.recipe, self.macros, self.targets)?;

        Ok(packager)
    }
}

fn logged(
    phase: job::Phase,
    is_pgo: bool,
    command: &str,
    f: impl FnOnce(&mut process::Command) -> &mut process::Command,
) -> Result<process::ExitStatus, io::Error> {
    let mut command = process::Command::new(command);

    f(&mut command);

    let mut child = command
        .stdout(process::Stdio::piped())
        .stderr(process::Stdio::piped())
        .spawn()?;

    // Log stdout and stderr
    let stdout_log = log(phase, is_pgo, child.stdout.take().unwrap());
    let stderr_log = log(phase, is_pgo, child.stderr.take().unwrap());

    // Forward SIGINT to this process
    ::container::forward_sigint(Pid::from_raw(child.id() as i32))?;

    let result = child.wait()?;

    let _ = stdout_log.join();
    let _ = stderr_log.join();

    Ok(result)
}

fn log<R>(phase: job::Phase, is_pgo: bool, pipe: R) -> thread::JoinHandle<()>
where
    R: io::Read + Send + 'static,
{
    use std::io::BufRead;

    thread::spawn(move || {
        let pgo = is_pgo.then_some("│").unwrap_or_default().dim();
        let kind = phase.styled(format!("{}│", phase.abbrev()));
        let tag = format!("{}{pgo}{kind}", "│".dim());

        let mut lines = io::BufReader::new(pipe).lines();

        while let Some(Ok(line)) = lines.next() {
            println!("{tag} {line}");
        }
    })
}

pub fn format_profile(script: &Script) -> String {
    let env = script
        .env
        .as_deref()
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.starts_with("#!") && !line.starts_with("set -") && !line.starts_with("TERM="))
        .join("\n");

    let action_functions = script
        .resolved_actions
        .iter()
        .map(|(identifier, command)| format!("a_{identifier}() {{\n{command}\n}}\nexport -f a_{identifier}"))
        .join("\n");

    let definition_vars = script
        .resolved_definitions
        .iter()
        .map(|(identifier, var)| format!("d_{identifier}=\"{var}\"; export d_{identifier}"))
        .join("\n");

    format!("{env}\n{action_functions}\n{definition_vars}")
}

fn breakpoint_line(
    breakpoint: &Breakpoint,
    recipe: &Recipe,
    build_target: BuildTarget,
    phase: job::Phase,
) -> Option<usize> {
    let profile = recipe.build_target_profile_key(build_target);

    let has_key = |line: &str, key: &str| {
        line.split_once(':')
            .map_or(false, |(leading, _)| leading.trim().ends_with(key))
    };

    let mut lines = recipe
        .source
        .lines()
        .enumerate()
        // If no profile, we care about root keys (no leading whitespace),
        // otherwise it will be indented
        .filter(|(_, line)| {
            let indented = line.trim().chars().next() != line.chars().next();

            if profile.is_none() {
                !indented
            } else {
                indented
            }
        })
        // Skip lines occurring before profile, otherwise it's the
        // root profile
        .skip_while(|(_, line)| {
            if let Some(profile) = &profile {
                !has_key(line, profile)
            } else {
                false
            }
        });

    let phase = match phase {
        // Internal phase, no breakpoint will occur
        job::Phase::Prepare => return None,
        job::Phase::Setup => "setup",
        job::Phase::Build => "build",
        job::Phase::Install => "install",
        job::Phase::Check => "check",
        job::Phase::Workload => "workload",
    };

    lines.find_map(|(mut line_num, line)| {
        if has_key(line, phase) {
            // 0 based to 1 based
            line_num += 1;

            let (_, rest) = line.split_once(':').expect("line contains :");

            // If block, string starts on next line
            if rest.trim().starts_with('|') || rest.trim().starts_with('>') {
                line_num += 1;
            }

            Some(line_num + breakpoint.line_num)
        } else {
            None
        }
    })
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("no supported build targets for recipe")]
    NoBuildTargets,
    #[error("macros")]
    Macros(#[from] macros::Error),
    #[error("job")]
    Job(#[from] job::Error),
    #[error("profile")]
    Profile(#[from] profile::Error),
    #[error("root")]
    Root(#[from] root::Error),
    #[error("upstream")]
    Upstream(#[from] upstream::Error),
    #[error("container")]
    Container(#[from] container::Error),
    #[error("recipe")]
    Recipe(#[from] recipe::Error),
    #[error("create packager")]
    Package(#[from] package::Error),
    #[error("io")]
    Io(#[from] io::Error),
}

#[derive(Debug, Error)]
pub enum ExecError {
    #[error("failed with status code {0}")]
    Code(i32),
    #[error("stopped by signal {}", .0.as_str())]
    Signal(Signal),
    #[error("stopped by unknown signal")]
    UnknownSignal,
    #[error(transparent)]
    Nix(#[from] nix::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
}
