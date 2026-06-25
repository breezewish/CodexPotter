import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const repoRoot = path.resolve(__dirname, "..", "..");
const cliPath = path.join(repoRoot, "npm", "bin", "codex-potter.js");

async function writeFakeInstaller(bin, commandName, outputText) {
  const commandPath = path.join(
    bin,
    process.platform === "win32" ? `${commandName}.cmd` : commandName,
  );
  if (process.platform === "win32") {
    await fs.promises.writeFile(
      commandPath,
      `@echo off\r\nfor %%A in (%*) do echo %%~A>>%NPX_LOG%\r\necho ${outputText}\r\n`,
      "utf8",
    );
    return;
  }

  await fs.promises.writeFile(
    commandPath,
    `#!/usr/bin/env sh\nprintf '%s\\n' "$@" > "$NPX_LOG"\nprintf '%s\\n' '${outputText}'\n`,
    "utf8",
  );
  await fs.promises.chmod(commandPath, 0o755);
}

async function makeFixture() {
  const root = await fs.promises.mkdtemp(path.join(os.tmpdir(), "potter-setup-"));
  const home = path.join(root, "home");
  const xdg = path.join(root, "xdg");
  const bin = path.join(root, "bin");
  const gitConfig = path.join(root, "gitconfig");
  const npxLog = path.join(root, "npx-args.txt");

  await fs.promises.mkdir(home, { recursive: true });
  await fs.promises.mkdir(xdg, { recursive: true });
  await fs.promises.mkdir(bin, { recursive: true });

  await writeFakeInstaller(bin, "npx", "fake npx skills installer");
  await writeFakeInstaller(bin, "bunx", "fake bunx skills installer");

  return { root, home, xdg, bin, gitConfig, npxLog };
}

function cliEnv(fixture, overrides = {}) {
  return {
    ...process.env,
    FORCE_COLOR: "0",
    GIT_CONFIG_GLOBAL: fixture.gitConfig,
    HOME: fixture.home,
    NO_COLOR: "1",
    NPX_LOG: fixture.npxLog,
    PATH: `${fixture.bin}${path.delimiter}${process.env.PATH}`,
    USERPROFILE: fixture.home,
    XDG_CONFIG_HOME: fixture.xdg,
    ...overrides,
  };
}

function spawnCli(fixture, args, stdin, envOverrides = {}) {
  return spawn(process.execPath, [cliPath, ...args], {
    cwd: fixture.root,
    stdio: [stdin, "pipe", "pipe"],
    env: cliEnv(fixture, envOverrides),
  });
}

async function runCli(fixture, args, options = {}) {
  const child = spawnCli(
    fixture,
    args,
    options.input === undefined ? "ignore" : "pipe",
    options.env,
  );

  if (options.input !== undefined) {
    child.stdin.end(options.input);
  }

  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (chunk) => {
    stdout += chunk.toString("utf8");
  });
  child.stderr.on("data", (chunk) => {
    stderr += chunk.toString("utf8");
  });

  const [code, signal] = await once(child, "exit");
  return { code, signal, stdout, stderr };
}

function gitConfigPathValue(filePath) {
  return filePath.split(path.sep).join("/");
}

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function legacyProfilePath(fixture) {
  return path.join(fixture.home, ".codex", "agents", "potter_worker.toml");
}

async function writeLegacyProfile(fixture, content = "legacy profile\n") {
  const filePath = legacyProfilePath(fixture);
  await fs.promises.mkdir(path.dirname(filePath), { recursive: true });
  await fs.promises.writeFile(filePath, content, "utf8");
  return filePath;
}

async function cleanupFixture(fixture) {
  await fs.promises.rm(fixture.root, { recursive: true, force: true });
}

test("lists available commands when no command is provided", async () => {
  const fixture = await makeFixture();

  try {
    const result = await runCli(fixture, []);

    assert.equal(result.signal, null);
    assert.equal(result.code, 0);
    assert.match(result.stdout, /Available commands:/);
    assert.match(result.stdout, /setup\s+Configure CodexPotter for \$loop\./);
    assert.equal(result.stderr, "");
    assert.equal(fs.existsSync(fixture.npxLog), false);
  } finally {
    await cleanupFixture(fixture);
  }
});

test("setup --yes updates gitignore, skips absent legacy profile, and pipes the loop skill installer", async () => {
  const fixture = await makeFixture();

  try {
    const result = await runCli(fixture, ["setup", "--yes"]);
    const gitignorePath = path.join(fixture.xdg, "git", "ignore");

    assert.equal(result.signal, null);
    assert.equal(result.code, 0);
    assert.doesNotMatch(result.stdout, /Planned setup:/);
    assert.match(result.stdout, /Todo: Ignore \/.codexpotter in global gitignore/);
    assert.match(result.stdout, /Skip: Remove legacy subagent profile .*potter_worker\.toml/);
    assert.match(result.stdout, /Todo: Install \/ update skill:/);
    assert.match(
      result.stdout,
      /npx --yes skills add --yes -g https:\/\/github\.com\/breezewish\/CodexPotter\/tree\/v2 -a codex/,
    );
    assert.match(
      result.stdout,
      new RegExp(`✓ Added \\/\\.codexpotter to ${escapeRegExp(gitignorePath)}`),
    );
    assert.doesNotMatch(result.stdout, /\(global gitignore file\)/);
    assert.doesNotMatch(result.stdout, /✓ Removed legacy subagent profile .*potter_worker\.toml/);
    assert.doesNotMatch(result.stdout, /Confirmation skipped/);
    assert.doesNotMatch(result.stdout, /Running loop skill installer/);
    assert.match(result.stdout, /fake npx skills installer/);
    assert.doesNotMatch(result.stdout, /✓ Skill added/);
    assert.match(result.stdout, /✨ CodexPotter setup complete!/);
    assert.match(result.stdout, /Usage in Codex: \$loop <your_instruction>/);
    assert.equal(result.stderr, "");

    assert.equal(await fs.promises.readFile(gitignorePath, "utf8"), "/.codexpotter\n");
    assert.equal(fs.existsSync(legacyProfilePath(fixture)), false);

    assert.equal(
      await fs.promises.readFile(fixture.npxLog, "utf8"),
      "--yes\nskills\nadd\n--yes\n-g\nhttps://github.com/breezewish/CodexPotter/tree/v2\n-a\ncodex\n",
    );
  } finally {
    await cleanupFixture(fixture);
  }
});

test("setup --yes removes legacy profile when present", async () => {
  const fixture = await makeFixture();

  try {
    const profilePath = await writeLegacyProfile(fixture);

    const result = await runCli(fixture, ["setup", "--yes"]);

    assert.equal(result.signal, null);
    assert.equal(result.code, 0);
    assert.match(result.stdout, /Todo: Remove legacy subagent profile .*potter_worker\.toml/);
    assert.match(result.stdout, /✓ Removed legacy subagent profile .*potter_worker\.toml/);
    assert.equal(result.stderr, "");
    assert.equal(fs.existsSync(profilePath), false);
  } finally {
    await cleanupFixture(fixture);
  }
});

test("setup uses bunx for the skill installer when launched by Bun", async () => {
  const fixture = await makeFixture();

  try {
    const result = await runCli(fixture, ["setup", "--yes"], {
      env: {
        npm_config_user_agent: "bun/1.3.0",
        npm_execpath: path.join(fixture.bin, "bun"),
      },
    });

    assert.equal(result.signal, null);
    assert.equal(result.code, 0);
    assert.match(
      result.stdout,
      /bunx skills add --yes -g https:\/\/github\.com\/breezewish\/CodexPotter\/tree\/v2 -a codex/,
    );
    assert.doesNotMatch(result.stdout, /npx --yes skills add/);
    assert.match(result.stdout, /fake bunx skills installer/);
    assert.equal(result.stderr, "");
    assert.equal(
      await fs.promises.readFile(fixture.npxLog, "utf8"),
      "skills\nadd\n--yes\n-g\nhttps://github.com/breezewish/CodexPotter/tree/v2\n-a\ncodex\n",
    );
  } finally {
    await cleanupFixture(fixture);
  }
});

test("setup waits for confirmation before writing files", async () => {
  const fixture = await makeFixture();

  try {
    const result = await runCli(fixture, ["setup"], { input: "\n" });

    assert.equal(result.signal, null);
    assert.equal(result.code, 0);
    assert.match(result.stdout, /Continue\? ● Yes \/ ○ No/);
    assert.match(result.stdout, /Setup cancelled\./);
    assert.equal(result.stderr, "");

    assert.equal(
      fs.existsSync(path.join(fixture.xdg, "git", "ignore")),
      false,
    );
    assert.equal(
      fs.existsSync(
        legacyProfilePath(fixture),
      ),
      false,
    );
    assert.equal(fs.existsSync(fixture.npxLog), false);
  } finally {
    await cleanupFixture(fixture);
  }
});

test("setup treats closed stdin as cancellation", async () => {
  const fixture = await makeFixture();

  try {
    const result = await runCli(fixture, ["setup"]);

    assert.equal(result.signal, null);
    assert.equal(result.code, 0);
    assert.match(result.stdout, /Continue\? ● Yes \/ ○ No/);
    assert.match(result.stdout, /Setup cancelled\./);
    assert.equal(result.stderr, "");
    assert.equal(
      fs.existsSync(path.join(fixture.xdg, "git", "ignore")),
      false,
    );
    assert.equal(fs.existsSync(fixture.npxLog), false);
  } finally {
    await cleanupFixture(fixture);
  }
});

test("setup accepts yes confirmation", async () => {
  const fixture = await makeFixture();

  try {
    const result = await runCli(fixture, ["setup"], { input: "yes\n" });

    assert.equal(result.signal, null);
    assert.equal(result.code, 0);
    assert.match(result.stdout, /Continue\? ● Yes \/ ○ No/);
    assert.match(result.stdout, /✨ CodexPotter setup complete!/);
    assert.equal(result.stderr, "");

    assert.equal(
      await fs.promises.readFile(path.join(fixture.xdg, "git", "ignore"), "utf8"),
      "/.codexpotter\n",
    );
    assert.equal(
      await fs.promises.readFile(fixture.npxLog, "utf8"),
      "--yes\nskills\nadd\n--yes\n-g\nhttps://github.com/breezewish/CodexPotter/tree/v2\n-a\ncodex\n",
    );
  } finally {
    await cleanupFixture(fixture);
  }
});

test("setup colors confirmation choices when color is enabled", async () => {
  const fixture = await makeFixture();

  try {
    const result = await runCli(fixture, ["setup"], {
      input: "n\n",
      env: {
        FORCE_COLOR: "1",
        NO_COLOR: "",
      },
    });

    assert.equal(result.signal, null);
    assert.equal(result.code, 0);
    assert.match(
      result.stdout,
      /\x1b\[32m●\x1b\[0m Yes\x1b\[2m \/ ○ No\x1b\[0m/,
    );
    assert.match(result.stdout, /Setup cancelled\./);
    assert.equal(result.stderr, "");
    assert.equal(fs.existsSync(path.join(fixture.xdg, "git", "ignore")), false);
    assert.equal(fs.existsSync(fixture.npxLog), false);
  } finally {
    await cleanupFixture(fixture);
  }
});

test("setup preserves global gitignore changes made during confirmation", async () => {
  const fixture = await makeFixture();

  try {
    const gitignorePath = path.join(fixture.xdg, "git", "ignore");
    await fs.promises.mkdir(path.dirname(gitignorePath), { recursive: true });
    await fs.promises.writeFile(gitignorePath, "before\n", "utf8");

    const child = spawnCli(fixture, ["setup"], "pipe");
    let stdout = "";
    let stderr = "";
    let confirmed = false;

    child.stdout.on("data", (chunk) => {
      stdout += chunk.toString("utf8");
      if (!confirmed && stdout.includes("Continue? ● Yes / ○ No")) {
        confirmed = true;
        fs.writeFileSync(gitignorePath, "before\nduring\n", "utf8");
        child.stdin.end("yes\n");
      }
    });
    child.stderr.on("data", (chunk) => {
      stderr += chunk.toString("utf8");
    });

    const [code, signal] = await once(child, "exit");

    assert.equal(confirmed, true);
    assert.equal(signal, null);
    assert.equal(code, 0);
    assert.match(
      stdout,
      new RegExp(`✓ Added \\/\\.codexpotter to ${escapeRegExp(gitignorePath)}`),
    );
    assert.equal(stderr, "");
    assert.equal(
      await fs.promises.readFile(gitignorePath, "utf8"),
      "before\nduring\n/.codexpotter\n",
    );
  } finally {
    await cleanupFixture(fixture);
  }
});

test("setup removes a legacy profile created during confirmation", async () => {
  const fixture = await makeFixture();

  try {
    const profilePath = legacyProfilePath(fixture);
    const child = spawnCli(fixture, ["setup"], "pipe");
    let stdout = "";
    let stderr = "";
    let confirmed = false;

    child.stdout.on("data", (chunk) => {
      stdout += chunk.toString("utf8");
      if (!confirmed && stdout.includes("Continue? ● Yes / ○ No")) {
        confirmed = true;
        fs.mkdirSync(path.dirname(profilePath), { recursive: true });
        fs.writeFileSync(profilePath, "created during confirmation\n", "utf8");
        child.stdin.end("yes\n");
      }
    });
    child.stderr.on("data", (chunk) => {
      stderr += chunk.toString("utf8");
    });

    const [code, signal] = await once(child, "exit");

    assert.equal(confirmed, true);
    assert.equal(signal, null);
    assert.equal(code, 0);
    assert.match(stdout, /✓ Removed legacy subagent profile .*potter_worker\.toml/);
    assert.equal(stderr, "");
    assert.equal(fs.existsSync(profilePath), false);
  } finally {
    await cleanupFixture(fixture);
  }
});

test("setup --yes does not duplicate existing gitignore and keeps legacy profile removed", async () => {
  const fixture = await makeFixture();

  try {
    const gitignorePath = path.join(fixture.xdg, "git", "ignore");
    await fs.promises.mkdir(path.dirname(gitignorePath), { recursive: true });
    await fs.promises.writeFile(gitignorePath, ".codexpotter/\n", "utf8");

    const result = await runCli(fixture, ["setup", "--yes"]);

    assert.equal(result.signal, null);
    assert.equal(result.code, 0);
    assert.match(result.stdout, /Skip: Ignore \/.codexpotter in global gitignore/);
    assert.match(result.stdout, /Skip: Remove legacy subagent profile .*potter_worker\.toml/);
    assert.doesNotMatch(result.stdout, /already has \.codexpotter entry/);
    assert.doesNotMatch(result.stdout, /already up to date \(/);
    assert.doesNotMatch(result.stdout, /Global gitignore already configured\./);
    assert.doesNotMatch(result.stdout, /Global gitignore updated\./);
    assert.doesNotMatch(result.stdout, /✓ Removed legacy subagent profile .*potter_worker\.toml/);
    assert.doesNotMatch(result.stdout, /Running loop skill installer/);
    assert.match(result.stdout, /Todo: Install \/ update skill:/);
    assert.match(
      result.stdout,
      /npx --yes skills add --yes -g https:\/\/github\.com\/breezewish\/CodexPotter\/tree\/v2 -a codex/,
    );
    assert.equal(result.stderr, "");

    assert.equal(await fs.promises.readFile(gitignorePath, "utf8"), ".codexpotter/\n");
    assert.equal(fs.existsSync(legacyProfilePath(fixture)), false);
    assert.equal(
      await fs.promises.readFile(fixture.npxLog, "utf8"),
      "--yes\nskills\nadd\n--yes\n-g\nhttps://github.com/breezewish/CodexPotter/tree/v2\n-a\ncodex\n",
    );
  } finally {
    await cleanupFixture(fixture);
  }
});

test("setup colors plan status labels when color is enabled", async () => {
  const fixture = await makeFixture();

  try {
    const gitignorePath = path.join(fixture.xdg, "git", "ignore");
    await fs.promises.mkdir(path.dirname(gitignorePath), { recursive: true });
    await fs.promises.writeFile(gitignorePath, ".codexpotter/\n", "utf8");
    await writeLegacyProfile(fixture);

    const result = await runCli(fixture, ["setup", "--yes"], {
      env: {
        FORCE_COLOR: "1",
        NO_COLOR: "",
      },
    });

    assert.equal(result.signal, null);
    assert.equal(result.code, 0);
    assert.match(result.stdout, /\x1b\[32m✓ Skip:\x1b\[0m/);
    assert.match(result.stdout, /\x1b\[33m□ Todo:\x1b\[0m/);
    assert.match(result.stdout, /\x1b\[2m\/.codexpotter\x1b\[0m/);
    assert.match(result.stdout, /\x1b\[2m.*potter_worker\.toml\x1b\[0m/);
    assert.match(
      result.stdout,
      /\x1b\[36mnpx --yes skills add --yes -g https:\/\/github\.com\/breezewish\/CodexPotter\/tree\/v2 -a codex\x1b\[0m/,
    );
    assert.doesNotMatch(result.stdout, /✓ Skill added/);
    assert.match(result.stdout, /\x1b\[32m✨ CodexPotter setup complete!\x1b\[0m/);
    assert.match(
      result.stdout,
      /\x1b\[1mUsage in Codex:\x1b\[0m \x1b\[36m\$loop\x1b\[0m <your_instruction>/,
    );
    assert.equal(result.stderr, "");
  } finally {
    await cleanupFixture(fixture);
  }
});

test("setup uses configured core.excludesfile before the XDG default", async () => {
  const fixture = await makeFixture();

  try {
    const configuredGitignorePath = path.join(fixture.home, "custom-ignore");
    await fs.promises.writeFile(
      fixture.gitConfig,
      `[core]\n\texcludesfile = ${gitConfigPathValue(configuredGitignorePath)}\n`,
      "utf8",
    );
    await fs.promises.writeFile(configuredGitignorePath, "existing\n", "utf8");

    const result = await runCli(fixture, ["setup", "--yes"]);

    assert.equal(result.signal, null);
    assert.equal(result.code, 0);
    assert.match(result.stdout, /Todo: Ignore \/.codexpotter in global gitignore/);
    assert.match(
      result.stdout,
      new RegExp(
        `✓ Added \\/\\.codexpotter to ${escapeRegExp(path.join("~", "custom-ignore"))}`,
      ),
    );
    assert.equal(result.stderr, "");

    assert.equal(
      await fs.promises.readFile(configuredGitignorePath, "utf8"),
      "existing\n/.codexpotter\n",
    );
    assert.equal(
      fs.existsSync(path.join(fixture.xdg, "git", "ignore")),
      false,
    );
  } finally {
    await cleanupFixture(fixture);
  }
});
