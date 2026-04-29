#!/usr/bin/env node

import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import readline from "node:readline";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

const CODEXPOTTER_GITIGNORE_ENTRY = "/.codexpotter";
const LOOP_SKILL_COMMAND = [
  "skills",
  "add",
  "--yes",
  "-g",
  "https://github.com/breezewish/CodexPotter/tree/v2",
  "-a",
  "codex",
];

const colorEnabled = shouldUseColor();
const colors = {
  bold: "\x1b[1m",
  cyan: "\x1b[36m",
  dim: "\x1b[2m",
  green: "\x1b[32m",
  red: "\x1b[31m",
  reset: "\x1b[0m",
  yellow: "\x1b[33m",
};

async function main(args) {
  const { command, yes, errors } = parseArgs(args);
  if (errors.length > 0) {
    printErrors(errors);
    printAvailableCommands();
    return 1;
  }

  if (command !== "setup") {
    if (command) {
      printErrors([`Unknown command: ${command}`]);
    }
    printAvailableCommands();
    return command ? 1 : 0;
  }

  await runSetup({ yes });
  return 0;
}

function parseArgs(args) {
  const remaining = [];
  const errors = [];
  let yes = false;

  for (const arg of args) {
    if (arg === "--yes" || arg === "-y") {
      yes = true;
    } else if (arg === "--help" || arg === "-h") {
      return { command: null, yes, errors };
    } else if (arg.startsWith("-")) {
      errors.push(`Unknown option: ${arg}`);
    } else {
      remaining.push(arg);
    }
  }

  if (remaining.length > 1) {
    errors.push(`Unexpected argument: ${remaining[1]}`);
  }

  return { command: remaining[0] ?? null, yes, errors };
}

function printAvailableCommands() {
  console.log(format("CodexPotter", "bold"));
  console.log("");
  console.log("Usage:");
  console.log("  codex-potter setup [--yes]");
  console.log("");
  console.log("Available commands:");
  console.log("  setup    Configure CodexPotter for $loop.");
}

function printErrors(errors) {
  for (const error of errors) {
    console.error(`${format("Error:", "red")} ${error}`);
  }
}

async function runSetup({ yes }) {
  const home = getHomeDir();
  const resourcePath = path.join(
    __dirname,
    "..",
    "resources",
    "potter_worker.toml",
  );
  const profilePath = path.join(home, ".codex", "agents", "potter_worker.toml");
  const profileContent = await fs.promises.readFile(resourcePath, "utf8");

  const globalGitignore = resolveGlobalGitignore(home);
  const gitignoreContent = await readTextIfExists(globalGitignore.path);
  const gitignoreNeedsWrite = !gitignoreIgnoresCodexPotter(gitignoreContent);

  const currentProfileContent = await readTextIfExists(profilePath);
  const profileNeedsWrite = currentProfileContent !== profileContent;
  const skillInstaller = resolveSkillInstaller();

  printSetupPlan({
    gitignoreNeedsWrite,
    profileNeedsWrite,
    profilePath,
    skillInstaller,
  });

  if (!yes && !(await confirm())) {
    console.log("Setup cancelled.");
    return;
  }

  if (gitignoreNeedsWrite) {
    const gitignoreUpdated = await ensureCodexPotterIgnored(globalGitignore.path);
    if (gitignoreUpdated) {
      console.log(
        `${format("✓ Added", "green")} ${format(
          CODEXPOTTER_GITIGNORE_ENTRY,
          "dim",
        )} to ${format(displayPath(globalGitignore.path), "dim")}`,
      );
    }
  }

  if (profileNeedsWrite) {
    await fs.promises.mkdir(path.dirname(profilePath), { recursive: true });
    await fs.promises.writeFile(profilePath, profileContent, "utf8");
    console.log(
      `${format("✓ Added", "green")} subagent profile ${format(
        displayPath(profilePath),
        "dim",
      )}`,
    );
  }

  await runLoopSkillInstaller(skillInstaller);

  console.log("");
  console.log(format("✨ CodexPotter setup complete!", "green"));
  console.log(
    `${format("Usage in Codex:", "bold")} ${format(
      "$loop",
      "cyan",
    )} <your_instruction>`,
  );
}

function printSetupPlan({
  gitignoreNeedsWrite,
  profileNeedsWrite,
  profilePath,
  skillInstaller,
}) {
  console.log(format("CodexPotter setup", "bold"));
  console.log("");
  console.log(
    `${statusLabel(gitignoreNeedsWrite)} Ignore ${format(
      CODEXPOTTER_GITIGNORE_ENTRY,
      "dim",
    )} in global gitignore`,
  );
  console.log(
    `${statusLabel(profileNeedsWrite)} Add subagent profile ${format(
      displayPath(profilePath),
      "dim",
    )}`,
  );
  console.log(
    `${statusLabel(true)} Install / update skill: ${format(
      skillInstaller.displayCommand,
      "cyan",
    )}`,
  );
  console.log("");
}

function statusLabel(needsAction) {
  return needsAction ? format("□ Todo:", "yellow") : format("✓ Skip:", "green");
}

async function confirm() {
  if (process.stdin.isTTY && process.stdout.isTTY) {
    return confirmInteractively();
  }

  return confirmFromLineInput();
}

async function confirmInteractively() {
  return await new Promise((resolve) => {
    let selected = true;
    let settled = false;
    const input = process.stdin;

    const render = () => {
      process.stdout.write(`\r\x1b[2K${confirmPrompt(selected)}`);
    };

    const settle = (value) => {
      if (settled) {
        return;
      }
      settled = true;
      input.off("keypress", onKeypress);
      input.off("close", onClose);
      input.off("end", onClose);
      if (input.isTTY) {
        input.setRawMode(false);
      }
      process.stdout.write("\n");
      resolve(value);
    };

    const onClose = () => {
      settle(false);
    };

    const onKeypress = (text, key = {}) => {
      if (key.ctrl && key.name === "c") {
        settle(false);
      } else if (key.name === "return") {
        settle(selected);
      } else if (key.name === "escape") {
        settle(false);
      } else if (key.name === "left" || key.name === "up") {
        selected = true;
        render();
      } else if (key.name === "right" || key.name === "down") {
        selected = false;
        render();
      } else if (key.name === "tab" || text === " ") {
        selected = !selected;
        render();
      } else if (text && text.toLowerCase() === "y") {
        settle(true);
      } else if (text && text.toLowerCase() === "n") {
        settle(false);
      }
    };

    readline.emitKeypressEvents(input);
    input.on("keypress", onKeypress);
    input.on("close", onClose);
    input.on("end", onClose);
    input.setRawMode(true);
    input.resume();
    render();
  });
}

async function confirmFromLineInput() {
  const answer = await new Promise((resolve) => {
    let settled = false;
    const rl = readline.createInterface({
      input: process.stdin,
      output: process.stdout,
      terminal: false,
    });

    const settle = (value) => {
      if (settled) {
        return;
      }
      settled = true;
      rl.close();
      resolve(value);
    };

    rl.on("close", () => {
      settle("");
    });
    rl.question(confirmPrompt(true), settle);
  });

  return /^(y|yes)$/i.test(String(answer).trim());
}

function confirmPrompt(yesSelected) {
  const yes = yesSelected
    ? `${format("●", "green")} Yes`
    : format("○ Yes", "dim");
  const no = yesSelected
    ? format(" / ○ No", "dim")
    : ` ${format("/", "dim")} ${format("●", "green")} No`;
  return `Continue? ${yes}${no} `;
}

function resolveGlobalGitignore(home) {
  const configured = spawnSync(
    "git",
    ["config", "--global", "--path", "--get", "core.excludesfile"],
    { encoding: "utf8" },
  );

  if (configured.status === 0) {
    const configuredPath = configured.stdout.trim();
    if (configuredPath) {
      const resolvedPath = expandHome(configuredPath, home);
      return {
        path: resolvedPath,
      };
    }
  }

  const configHome = process.env.XDG_CONFIG_HOME || path.join(home, ".config");
  const resolvedPath = path.join(configHome, "git", "ignore");
  return {
    path: resolvedPath,
  };
}

async function readTextIfExists(filePath) {
  try {
    return await fs.promises.readFile(filePath, "utf8");
  } catch (error) {
    if (error && error.code === "ENOENT") {
      return "";
    }
    throw error;
  }
}

async function ensureCodexPotterIgnored(filePath) {
  const currentContent = await readTextIfExists(filePath);
  if (gitignoreIgnoresCodexPotter(currentContent)) {
    return false;
  }

  let updated = currentContent;
  if (updated && !updated.endsWith("\n")) {
    updated += "\n";
  }
  updated += `${CODEXPOTTER_GITIGNORE_ENTRY}\n`;
  await writeTextAtomic(filePath, updated);
  return true;
}

async function writeTextAtomic(filePath, content) {
  const dir = path.dirname(filePath);
  await fs.promises.mkdir(dir, { recursive: true });
  const tempPath = path.join(
    dir,
    `.${path.basename(filePath)}.${process.pid}.${Date.now()}.tmp`,
  );

  try {
    await fs.promises.writeFile(tempPath, content, "utf8");
    await fs.promises.rename(tempPath, filePath);
  } catch (error) {
    await fs.promises.rm(tempPath, { force: true }).catch(() => {});
    throw error;
  }
}

function gitignoreIgnoresCodexPotter(contents) {
  if (!contents) {
    return false;
  }

  return simpleGitignoreIgnoresCodexPotter(contents);
}

function simpleGitignoreIgnoresCodexPotter(contents) {
  let ignored = false;

  for (const rawLine of contents.split(/\r?\n/)) {
    const parsed = parseGitignoreLine(rawLine);
    if (!parsed) {
      continue;
    }

    if (patternMatchesCodexPotter(parsed.pattern)) {
      ignored = !parsed.negated;
    }
  }

  return ignored;
}

function parseGitignoreLine(rawLine) {
  const line = rawLine.trim();
  if (!line || line.startsWith("#")) {
    return null;
  }

  if (line.startsWith("!")) {
    return { negated: true, pattern: line.slice(1) };
  }

  return { negated: false, pattern: line };
}

function patternMatchesCodexPotter(pattern) {
  const normalized = pattern.replace(/\\/g, "/").replace(/^\/+/, "");
  const withoutTrailingSlash = normalized.replace(/\/+$/, "");

  return (
    withoutTrailingSlash === ".codexpotter" ||
    withoutTrailingSlash === "**/.codexpotter" ||
    normalized === ".codexpotter/**" ||
    normalized === "**/.codexpotter/**"
  );
}

function resolveSkillInstaller() {
  const runner = detectPackageRunner();
  const args =
    runner === "npx" ? ["--yes", ...LOOP_SKILL_COMMAND] : LOOP_SKILL_COMMAND;
  return {
    command: executableForRunner(runner),
    args,
    displayCommand: `${runner} ${args.join(" ")}`,
  };
}

function detectPackageRunner() {
  const userAgent = process.env.npm_config_user_agent || "";
  const execPath = process.env.npm_execpath || "";
  const execName = path.basename(execPath).toLowerCase();
  if (/\bbun\//.test(userAgent) || execName.startsWith("bun")) {
    return "bunx";
  }
  return "npx";
}

function executableForRunner(runner) {
  if (process.platform === "win32") {
    return `${runner}.cmd`;
  }
  return runner;
}

async function runLoopSkillInstaller(skillInstaller) {
  const child = spawn(skillInstaller.command, skillInstaller.args, {
    stdio: "inherit",
  });

  const result = await new Promise((resolve, reject) => {
    child.on("error", reject);
    child.on("exit", (code, signal) => {
      resolve({ code, signal });
    });
  });

  if (result.signal) {
    throw new Error(`Loop skill installer terminated by ${result.signal}.`);
  }

  if (result.code !== 0) {
    throw new Error(`Loop skill installer failed with exit code ${result.code}.`);
  }
}

function getHomeDir() {
  const home = process.env.HOME || process.env.USERPROFILE;
  if (!home) {
    throw new Error("Cannot determine the home directory.");
  }
  return home;
}

function expandHome(value, home) {
  if (value === "~") {
    return home;
  }
  if (value.startsWith("~/") || value.startsWith("~\\")) {
    return path.join(home, value.slice(2));
  }
  return value;
}

function displayPath(filePath) {
  const home = process.env.HOME || process.env.USERPROFILE;
  if (!home) {
    return filePath;
  }

  const relative = path.relative(home, filePath);
  if (relative === "") {
    return "~";
  }
  if (!relative.startsWith("..") && !path.isAbsolute(relative)) {
    return path.join("~", relative);
  }
  return filePath;
}

function shouldUseColor() {
  if (process.env.NO_COLOR) {
    return false;
  }
  if (process.env.FORCE_COLOR && process.env.FORCE_COLOR !== "0") {
    return true;
  }
  return Boolean(process.stdout.isTTY);
}

function format(text, color) {
  if (!colorEnabled) {
    return text;
  }
  return `${colors[color]}${text}${colors.reset}`;
}

try {
  const exitCode = await main(process.argv.slice(2));
  process.exit(exitCode);
} catch (error) {
  console.error(`${format("Error:", "red")} ${error.message}`);
  process.exit(1);
}
