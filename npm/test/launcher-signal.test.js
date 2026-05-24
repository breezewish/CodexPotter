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

function currentTargetTriple() {
  const { platform, arch } = process;

  switch (platform) {
    case "linux":
    case "android":
      switch (arch) {
        case "x64":
          return "x86_64-unknown-linux-musl";
        case "arm64":
          return "aarch64-unknown-linux-musl";
        default:
          break;
      }
      break;
    case "darwin":
      switch (arch) {
        case "x64":
          return "x86_64-apple-darwin";
        case "arm64":
          return "aarch64-apple-darwin";
        default:
          break;
      }
      break;
    case "win32":
      switch (arch) {
        case "x64":
          return "x86_64-pc-windows-msvc";
        case "arm64":
          return "aarch64-pc-windows-msvc";
        default:
          break;
      }
      break;
    default:
      break;
  }

  throw new Error(`Unsupported platform: ${platform} (${arch})`);
}

async function makeTempDir(prefix) {
  return fs.promises.mkdtemp(path.join(os.tmpdir(), prefix));
}

async function waitForSubstring(stream, substring, timeoutMs) {
  return new Promise((resolve, reject) => {
    let buffer = "";
    const timeout = setTimeout(() => {
      cleanup();
      reject(new Error(`Timed out waiting for ${substring}`));
    }, timeoutMs);

    function cleanup() {
      clearTimeout(timeout);
      stream.off("data", onData);
      stream.off("error", onError);
    }

    function onError(err) {
      cleanup();
      reject(err);
    }

    function onData(chunk) {
      buffer += chunk.toString("utf8");
      if (buffer.includes(substring)) {
        cleanup();
        resolve();
      }
    }

    stream.on("data", onData);
    stream.on("error", onError);
  });
}

function launcherSourcePath() {
  return path.join(repoRoot, "npm", "bin", "codex-potter.js");
}

function launcherPackageJsonSourcePath() {
  return path.join(repoRoot, "npm", "package.json");
}

async function stageLauncherPackage(tmp) {
  const launcherDir = path.join(tmp, "bin");
  const launcherPath = path.join(launcherDir, "codex-potter.js");

  await fs.promises.mkdir(launcherDir, { recursive: true });
  await fs.promises.copyFile(launcherSourcePath(), launcherPath);
  await fs.promises.copyFile(
    launcherPackageJsonSourcePath(),
    path.join(tmp, "package.json"),
  );

  return launcherPath;
}

test(
  "npm launcher preserves bun reinstall hint when optional package is missing",
  { timeout: 10_000 },
  async () => {
    const tmp = await makeTempDir("codex-potter-launcher-");
    let launcherProcess = null;

    try {
      const launcherPath = await stageLauncherPackage(tmp);

      launcherProcess = spawn(process.execPath, [launcherPath], {
        stdio: ["ignore", "pipe", "pipe"],
        env: {
          ...process.env,
          npm_config_user_agent: "bun/1.2.0",
        },
      });

      let output = "";
      launcherProcess.stdout.on("data", (chunk) => {
        output += chunk.toString("utf8");
      });
      launcherProcess.stderr.on("data", (chunk) => {
        output += chunk.toString("utf8");
      });

      const [code, signal] = await once(launcherProcess, "exit");
      assert.equal(signal, null);
      assert.equal(code, 1);
      assert.match(output, /Missing optional dependency/);
      assert.match(output, /bun install -g codex-potter@latest/);
    } finally {
      if (
        launcherProcess &&
        launcherProcess.exitCode === null &&
        launcherProcess.signalCode === null
      ) {
        launcherProcess.kill("SIGKILL");
        await once(launcherProcess, "exit");
      }
      await fs.promises.rm(tmp, { recursive: true, force: true });
    }
  },
);

test(
  "npm launcher mirrors child SIGTERM exit semantics",
  {
    skip:
      process.platform === "win32"
        ? "SIGTERM behavior is not reliable on win32 runners"
        : false,
    timeout: 10_000,
  },
  async () => {
    const tmp = await makeTempDir("codex-potter-launcher-");
    let launcherProcess = null;

    try {
      const triple = currentTargetTriple();
      const launcherPath = await stageLauncherPackage(tmp);
      const vendorBinaryDir = path.join(
        tmp,
        "vendor",
        triple,
        "codex-potter",
      );
      const vendorBinaryPath = path.join(vendorBinaryDir, "codex-potter");

      await fs.promises.mkdir(vendorBinaryDir, { recursive: true });

      await fs.promises.writeFile(
        vendorBinaryPath,
        "#!/usr/bin/env node\n\nconsole.log(\"__CODEX_POTTER_TEST_READY__\");\n\nsetTimeout(() => {\n  console.log(\"__CODEX_POTTER_TEST_KILLING__\");\n  process.kill(process.pid, \"SIGTERM\");\n}, 50);\n\nsetTimeout(() => {\n  console.log(\"__CODEX_POTTER_TEST_STILL_ALIVE__\");\n}, 500);\n",
        "utf8",
      );
      await fs.promises.chmod(vendorBinaryPath, 0o755);

      launcherProcess = spawn(process.execPath, [launcherPath], {
        stdio: ["ignore", "pipe", "pipe"],
      });

      let output = "";
      launcherProcess.stdout.on("data", (chunk) => {
        output += chunk.toString("utf8");
      });
      launcherProcess.stderr.on("data", (chunk) => {
        output += chunk.toString("utf8");
      });

      const exitPromise = once(launcherProcess, "exit");
      await waitForSubstring(
        launcherProcess.stdout,
        "__CODEX_POTTER_TEST_READY__",
        2_000,
      );

      const [code, signal] = await Promise.race([
        exitPromise,
        new Promise((_, reject) => {
          setTimeout(() => {
            reject(
              new Error(
                `Timed out waiting for launcher exit. Output:\n${output}`,
              ),
            );
          }, 2_000);
        }),
      ]);
      assert.equal(code, null);
      assert.equal(signal, "SIGTERM");
    } finally {
      if (
        launcherProcess &&
        launcherProcess.exitCode === null &&
        launcherProcess.signalCode === null
      ) {
        launcherProcess.kill("SIGKILL");
        await once(launcherProcess, "exit");
      }
      await fs.promises.rm(tmp, { recursive: true, force: true });
    }
  },
);

test(
  "npm launcher mirrors child exit codes",
  {
    skip:
      process.platform === "win32"
        ? "dummy shell binary is not portable on win32 runners"
        : false,
    timeout: 10_000,
  },
  async () => {
    const tmp = await makeTempDir("codex-potter-launcher-");
    let launcherProcess = null;

    try {
      const triple = currentTargetTriple();
      const launcherPath = await stageLauncherPackage(tmp);
      const vendorBinaryDir = path.join(
        tmp,
        "vendor",
        triple,
        "codex-potter",
      );
      const vendorBinaryPath = path.join(vendorBinaryDir, "codex-potter");

      await fs.promises.mkdir(vendorBinaryDir, { recursive: true });

      await fs.promises.writeFile(
        vendorBinaryPath,
        "#!/usr/bin/env sh\n\nexit 42\n",
        "utf8",
      );
      await fs.promises.chmod(vendorBinaryPath, 0o755);

      launcherProcess = spawn(process.execPath, [launcherPath], {
        stdio: ["ignore", "pipe", "pipe"],
      });

      const exitPromise = once(launcherProcess, "exit");
      const [code, signal] = await exitPromise;
      assert.equal(signal, null);
      assert.equal(code, 42);
    } finally {
      if (
        launcherProcess &&
        launcherProcess.exitCode === null &&
        launcherProcess.signalCode === null
      ) {
        launcherProcess.kill("SIGKILL");
        await once(launcherProcess, "exit");
      }
      await fs.promises.rm(tmp, { recursive: true, force: true });
    }
  },
);
