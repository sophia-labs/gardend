/**
 * Long-running sandbox worker for prototype testing.
 *
 * Runs INSIDE the bwrap sandbox. Reads an init message with workload config,
 * then either:
 *   - runs the original synthetic loop workload,
 *   - executes a real command inside the sandbox, or
 *   - supervises a persistent language runtime that accepts incremental input.
 *
 * Plain .mjs — no build step, no TypeScript, bind-mounted into sandbox.
 */

import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { format, inspect } from "node:util";
import { Script, createContext } from "node:vm";

// ─── NDJSON helpers ──────────────────────────────────────────────────────

function emit(msg) {
	process.stdout.write(JSON.stringify(msg) + "\n");
}

function createControlChannel(timeoutMs = 5000) {
	const handlers = new Set();
	const closeHandlers = new Set();
	let initResolved = false;
	let initResolve;
	let initReject;

	const initPromise = new Promise((resolve, reject) => {
		initResolve = resolve;
		initReject = reject;
	});

	const timer = setTimeout(() => {
		if (!initResolved) {
			initReject(new Error("No init message received within timeout"));
		}
	}, timeoutMs);

	const rl = createInterface({ input: process.stdin, crlfDelay: Infinity });
	rl.on("line", (line) => {
		const trimmed = line.trim();
		if (!trimmed) return;

		let message;
		try {
			message = JSON.parse(trimmed);
		} catch {
			return;
		}

		if (message.kind === "init" && !initResolved) {
			initResolved = true;
			clearTimeout(timer);
			initResolve(message.config || {});
			return;
		}

		for (const handler of handlers) {
			handler(message);
		}
	});

	rl.on("close", () => {
		clearTimeout(timer);
		if (!initResolved) {
			initReject(new Error("stdin closed before init received"));
			return;
		}

		for (const handler of closeHandlers) {
			handler();
		}
	});

	return {
		readInit() {
			return initPromise;
		},
		subscribe(handler) {
			handlers.add(handler);
			return () => handlers.delete(handler);
		},
		onClose(handler) {
			closeHandlers.add(handler);
			return () => closeHandlers.delete(handler);
		},
	};
}

// ─── Workload helpers ────────────────────────────────────────────────────

function fibonacci(n) {
	if (n <= 1) return n;
	let a = 0;
	let b = 1;
	for (let i = 2; i <= n; i++) {
		[a, b] = [b, a + b];
	}
	return b;
}

function sleep(ms) {
	return new Promise((resolve) => setTimeout(resolve, ms));
}

function normalizeCommand(config) {
	if (!Array.isArray(config.command) || config.command.length === 0) {
		throw new Error("command workload requires a non-empty config.command array");
	}
	return config.command.map((part) => {
		if (typeof part !== "string" || !part) {
			throw new Error("command workload only supports non-empty string argv entries");
		}
		return part;
	});
}

function normalizeEnv(env) {
	if (env == null) return {};
	if (typeof env !== "object" || Array.isArray(env)) {
		throw new Error("command workload env must be an object");
	}

	return Object.fromEntries(
		Object.entries(env).map(([key, value]) => [key, String(value)]),
	);
}

function normalizeRuntime(config) {
	if (config.runtime !== "node" && config.runtime !== "python") {
		throw new Error(`unsupported runtime: ${String(config.runtime)}`);
	}
	return config.runtime;
}

function createRuntimeGateClient(channel) {
	let nextCallId = 0;
	let closed = false;
	const pending = new Map();

	const unsubscribe = channel.subscribe((message) => {
		if (message.kind !== "gate_result" || typeof message.call_id !== "string") {
			return;
		}
		const waiter = pending.get(message.call_id);
		if (!waiter) return;
		pending.delete(message.call_id);
		if (message.is_error) {
			waiter.reject(
				new Error(
					`CHOREOGRAPH_CAPABILITY_CALL_FAILED: ${waiter.capability}.${waiter.tool}: ${String(message.result)}`,
				),
			);
			return;
		}
		waiter.resolve(message.result);
	});

	const close = () => {
		if (closed) return;
		closed = true;
		unsubscribe();
		for (const waiter of pending.values()) {
			waiter.reject(new Error("CHOREOGRAPH_CAPABILITY_CHANNEL_CLOSED: runtime capability channel closed"));
		}
		pending.clear();
	};

	return {
		call(capability, tool, args = {}) {
			if (closed) {
				return Promise.reject(
					new Error("CHOREOGRAPH_CAPABILITY_CHANNEL_CLOSED: runtime capability channel closed"),
				);
			}
			if (typeof capability !== "string" || !capability.trim()) {
				return Promise.reject(new Error("CHOREOGRAPH_CAPABILITY_INVALID: capability must be a non-empty string"));
			}
			if (typeof tool !== "string" || !tool.trim()) {
				return Promise.reject(new Error("CHOREOGRAPH_CAPABILITY_INVALID: tool must be a non-empty string"));
			}
			let normalizedArgs;
			try {
				normalizedArgs = JSON.parse(JSON.stringify(args ?? {}));
			} catch {
				return Promise.reject(
					new Error("CHOREOGRAPH_CAPABILITY_INVALID: capability arguments must be JSON-serializable"),
				);
			}
			if (!normalizedArgs || typeof normalizedArgs !== "object" || Array.isArray(normalizedArgs)) {
				return Promise.reject(
					new Error("CHOREOGRAPH_CAPABILITY_INVALID: capability arguments must be an object"),
				);
			}

			const callId = `runtime-gate-${nextCallId++}`;
			return new Promise((resolve, reject) => {
				pending.set(callId, { resolve, reject, capability, tool });
				emit({
					kind: "gate_call",
					call_id: callId,
					capability,
					tool,
					args: normalizedArgs,
				});
			});
		},
		close,
	};
}

function createNodeRuntimeEvaluator(invokeCapability) {
	const context = createContext({});
	context.globalThis = context;
	Object.defineProperty(context, "__choreographInvoke", {
		value: invokeCapability,
		writable: false,
		configurable: false,
		enumerable: false,
	});

	return {
		async execute(code) {
			const stdout = [];
			const stderr = [];
			context.console = {
				log: (...args) => stdout.push(format(...args)),
				info: (...args) => stdout.push(format(...args)),
				warn: (...args) => stderr.push(format(...args)),
				error: (...args) => stderr.push(format(...args)),
			};

			try {
				let result = new Script(code, { displayErrors: true }).runInContext(context);
				if (result && typeof result.then === "function") {
					result = await result;
				}
				return {
					ok: true,
					stdout: stdout.join("\n"),
					stderr: stderr.join("\n"),
					has_result: result !== undefined,
					result: result === undefined ? undefined : inspect(result, { depth: 4, colors: false }),
					result_json: serializeRuntimeResult(result),
				};
			} catch (error) {
				return {
					ok: false,
					stdout: stdout.join("\n"),
					stderr: stderr.join("\n"),
					error: error && error.stack ? error.stack : String(error),
				};
			}
		},
	};
}

function serializeRuntimeResult(result) {
	if (result === undefined) return undefined;
	try {
		return JSON.stringify(result);
	} catch {
		return undefined;
	}
}

function spawnPythonRuntimeBridge(config) {
	const bridgeCmd = config.bridge_command;
	if (!Array.isArray(bridgeCmd) || bridgeCmd.length === 0) {
		throw new Error("python runtime config missing bridge_command");
	}
	for (const part of bridgeCmd) {
		if (typeof part !== "string" || part.length === 0) {
			throw new Error("python runtime bridge_command must contain non-empty strings");
		}
	}
	const env = {
		...process.env,
		...normalizeEnv(config.env),
	};
	const cwd = typeof config.cwd === "string" ? config.cwd : undefined;

	return {
		runtime: "python",
		child: spawn(bridgeCmd[0], bridgeCmd.slice(1), {
			cwd,
			env,
			stdio: ["pipe", "pipe", "pipe"],
		}),
	};
}

async function runLoopWorkload(config) {
	const maxIterations = config.max_iterations ?? 100;
	const sleepMs = config.sleep_ms ?? 1000;
	const memoryAllocMb = config.memory_alloc_mb ?? 0;
	const fibonacciN = config.fibonacci_n ?? 0;

	emit({
		kind: "event",
		event: {
			type: "started",
			mode: "loop",
			config: { maxIterations, sleepMs, memoryAllocMb, fibonacciN },
			pid: process.pid,
		},
	});

	const buffers = [];

	for (let i = 1; i <= maxIterations; i++) {
		if (memoryAllocMb > 0) {
			const buf = Buffer.alloc(memoryAllocMb * 1024 * 1024);
			buf.fill(i % 256);
			buffers.push(buf);
		}

		if (fibonacciN > 0) {
			fibonacci(fibonacciN);
		}

		const mem = process.memoryUsage();
		emit({
			kind: "event",
			event: {
				type: "heartbeat",
				iteration: i,
				pid: process.pid,
				uptime: process.uptime(),
				memory: {
					rss: mem.rss,
					heapUsed: mem.heapUsed,
					heapTotal: mem.heapTotal,
					external: mem.external,
					arrayBuffers: mem.arrayBuffers,
				},
				buffersAllocated: buffers.length,
			},
		});

		if (i < maxIterations) {
			await sleep(sleepMs);
		}
	}

	const finalMem = process.memoryUsage();
	emit({
		kind: "result",
		result: {
			status: "done",
			mode: "loop",
			iterations_completed: maxIterations,
			peak_memory: {
				rss: finalMem.rss,
				heapUsed: finalMem.heapUsed,
			},
		},
	});

	return 0;
}

async function runCommandWorkload(config) {
	const command = normalizeCommand(config);
	const cwd = typeof config.cwd === "string" ? config.cwd : undefined;
	const env = {
		...process.env,
		...normalizeEnv(config.env),
	};

	return new Promise((resolve, reject) => {
		const child = spawn(command[0], command.slice(1), {
			cwd,
			env,
			stdio: ["ignore", "pipe", "pipe"],
		});

		let stdoutBytes = 0;
		let stderrBytes = 0;

		child.once("spawn", () => {
			emit({
				kind: "event",
				event: {
					type: "started",
					mode: "command",
					pid: process.pid,
					child_pid: child.pid,
					command,
					cwd,
				},
			});
		});

		child.once("error", reject);

		child.stdout?.on("data", (chunk) => {
			const text = chunk.toString("utf8");
			stdoutBytes += Buffer.byteLength(text);
			emit({
				kind: "event",
				event: {
					type: "stdout",
					text,
				},
			});
		});

		child.stderr?.on("data", (chunk) => {
			const text = chunk.toString("utf8");
			stderrBytes += Buffer.byteLength(text);
			emit({
				kind: "event",
				event: {
					type: "stderr",
					text,
				},
			});
		});

		child.once("close", (code, signal) => {
			const exitCode = typeof code === "number" ? code : 1;
			emit({
				kind: "result",
				result: {
					status: exitCode === 0 ? "done" : "error",
					mode: "command",
					command,
					cwd,
					exit_code: exitCode,
					signal,
					stdout_bytes: stdoutBytes,
					stderr_bytes: stderrBytes,
				},
			});
			resolve(exitCode);
		});
	});
}

async function runRuntimeWorkload(config, channel) {
	const runtime = normalizeRuntime(config);
	const bootstrapCode = typeof config.bootstrap_code === "string" ? config.bootstrap_code : undefined;

	if (runtime === "node") {
		const gateClient = createRuntimeGateClient(channel);
		const evaluator = createNodeRuntimeEvaluator((capability, tool, args) =>
			gateClient.call(capability, tool, args),
		);
		return new Promise((resolve) => {
			let closed = false;
			let queue = Promise.resolve();

			const emitExecResult = (inputId, message) => {
				if (typeof message.stdout === "string" && message.stdout.length > 0) {
					emit({
						kind: "event",
						event: {
							type: "runtime_stdout",
							input_id: inputId,
							text: message.stdout,
						},
					});
				}
				if (typeof message.stderr === "string" && message.stderr.length > 0) {
					emit({
						kind: "event",
						event: {
							type: "runtime_stderr",
							input_id: inputId,
							text: message.stderr,
						},
					});
				}

				const resultEvent = {
					type: "runtime_result",
					input_id: inputId,
					ok: Boolean(message.ok),
				};
				if (message.has_result && typeof message.result === "string") {
					resultEvent.result = message.result;
				}
				if (message.has_result && typeof message.result_json === "string") {
					resultEvent.result_json = message.result_json;
				}
				if (!message.ok && typeof message.error === "string") {
					resultEvent.error = message.error;
				}
				emit({
					kind: "event",
					event: resultEvent,
				});
			};

			const execute = (inputId, code) => {
				queue = queue.then(async () => {
					if (closed) {
						return;
					}
					emit({
						kind: "event",
						event: {
							type: "runtime_busy",
							input_id: inputId,
						},
					});
					const result = await evaluator.execute(code);
					emitExecResult(inputId, result);
				});
			};

			const finish = () => {
				if (closed) {
					return;
				}
				closed = true;
				queue = queue.finally(() => {
					gateClient.close();
					emit({
						kind: "event",
						event: {
							type: "runtime_exit",
							runtime,
							exit_code: 0,
							signal: null,
						},
					});
					emit({
						kind: "result",
						result: {
							status: "done",
							mode: "runtime",
							runtime,
							exit_code: 0,
							signal: null,
						},
					});
					resolve(0);
				});
			};

			emit({
				kind: "event",
				event: {
					type: "started",
					mode: "runtime",
					runtime,
					pid: process.pid,
				},
			});
			emit({
				kind: "event",
				event: {
					type: "runtime_ready",
					runtime,
				},
			});
			if (bootstrapCode) {
				execute("bootstrap-0", bootstrapCode);
			}

			const unsubscribe = channel.subscribe((message) => {
				if (message.kind === "runtime_input" && typeof message.code === "string") {
					execute(message.input_id || "runtime", message.code);
					return;
				}

				if (message.kind === "runtime_close") {
					finish();
				}
			});

			const unsubscribeClose = channel.onClose(() => {
				finish();
			});

			queue.finally(() => {
				if (!closed) {
					return;
				}
				unsubscribe();
				unsubscribeClose();
			});
		});
	}

	const { child } = spawnPythonRuntimeBridge(config);
	return new Promise((resolve, reject) => {
		let bootstrapped = false;
		let closed = false;

		const sendExec = (inputId, code) => {
			if (closed || child.stdin?.destroyed) {
				return;
			}
			emit({
				kind: "event",
				event: {
					type: "runtime_busy",
					input_id: inputId,
				},
			});
			child.stdin.write(`${JSON.stringify({ kind: "exec", input_id: inputId, code })}\n`);
		};

		child.once("spawn", () => {
			emit({
				kind: "event",
				event: {
					type: "started",
					mode: "runtime",
					runtime,
					pid: process.pid,
					child_pid: child.pid,
				},
			});
		});

		child.once("error", reject);

		if (child.stderr) {
			child.stderr.on("data", (chunk) => {
				const text = chunk.toString("utf8");
				if (!text) {
					return;
				}
				emit({
					kind: "event",
					event: {
						type: "stderr",
						text,
					},
				});
			});
		}

		const bridgeOutput = createInterface({ input: child.stdout, crlfDelay: Infinity });
		bridgeOutput.on("line", (line) => {
			const trimmed = line.trim();
			if (!trimmed) {
				return;
			}

			let message;
			try {
				message = JSON.parse(trimmed);
			} catch {
				return;
			}

			if (message.kind === "ready") {
				emit({
					kind: "event",
					event: {
						type: "runtime_ready",
						runtime,
					},
				});
				if (!bootstrapped && bootstrapCode) {
					bootstrapped = true;
					sendExec("bootstrap-0", bootstrapCode);
				}
				return;
			}

			if (message.kind !== "exec_result") {
				return;
			}

			const inputId = typeof message.input_id === "string" ? message.input_id : "runtime";
			if (typeof message.stdout === "string" && message.stdout.length > 0) {
				emit({
					kind: "event",
					event: {
						type: "runtime_stdout",
						input_id: inputId,
						text: message.stdout,
					},
				});
			}
			if (typeof message.stderr === "string" && message.stderr.length > 0) {
				emit({
					kind: "event",
					event: {
						type: "runtime_stderr",
						input_id: inputId,
						text: message.stderr,
					},
				});
			}

			const resultEvent = {
				type: "runtime_result",
				input_id: inputId,
				ok: Boolean(message.ok),
			};
			if (message.has_result && typeof message.result === "string") {
				resultEvent.result = message.result;
			}
			if (message.has_result && typeof message.result_json === "string") {
				resultEvent.result_json = message.result_json;
			}
			if (!message.ok && typeof message.error === "string") {
				resultEvent.error = message.error;
			}
			emit({
				kind: "event",
				event: resultEvent,
			});
		});

		const unsubscribe = channel.subscribe((message) => {
			if (closed) {
				return;
			}

			if (message.kind === "runtime_input" && typeof message.code === "string") {
				sendExec(message.input_id || "runtime", message.code);
				return;
			}

			if (message.kind === "runtime_close") {
				closed = true;
				if (child.stdin?.destroyed) {
					child.kill("SIGTERM");
					return;
				}
				child.stdin.write(`${JSON.stringify({ kind: "close" })}\n`);
				child.stdin.end();
			}
		});

		const unsubscribeClose = channel.onClose(() => {
			if (closed) {
				return;
			}
			closed = true;
			child.kill("SIGTERM");
		});

		child.once("close", (code, signal) => {
			unsubscribe();
			unsubscribeClose();
			bridgeOutput.close();
			const exitCode = typeof code === "number" ? code : 1;
			emit({
				kind: "event",
				event: {
					type: "runtime_exit",
					runtime,
					exit_code: code,
					signal,
				},
			});
			emit({
				kind: "result",
				result: {
					status: exitCode === 0 ? "done" : "error",
					mode: "runtime",
					runtime,
					exit_code: exitCode,
					signal,
				},
			});
			resolve(exitCode);
		});
	});
}

// ─── Kernel workload ─────────────────────────────────────────────────────

async function runKernelWorkload(config, channel) {
	const bridgeCmd = config.bridge_command;
	if (!Array.isArray(bridgeCmd) || bridgeCmd.length === 0) {
		throw new Error("kernel config missing bridge_command");
	}

	const kernel = typeof config.kernel === "string" ? config.kernel : "python3";
	const env = {
		...process.env,
		...normalizeEnv(config.env),
	};
	const cwd = typeof config.cwd === "string" ? config.cwd : undefined;

	const child = spawn(bridgeCmd[0], bridgeCmd.slice(1), {
		cwd,
		env,
		stdio: ["pipe", "pipe", "pipe"],
	});

	return new Promise((resolve, reject) => {
		let bootstrapped = false;
		let closed = false;
		const bootstrapCode = typeof config.bootstrap_code === "string" ? config.bootstrap_code : undefined;

		const sendExec = (inputId, code) => {
			if (closed || child.stdin?.destroyed) return;
			child.stdin.write(`${JSON.stringify({ kind: "exec", input_id: inputId, code })}\n`);
		};

		child.once("spawn", () => {
			emit({
				kind: "event",
				event: {
					type: "started",
					mode: "kernel",
					runtime: kernel,
					pid: process.pid,
					child_pid: child.pid,
				},
			});
		});

		child.once("error", reject);

		if (child.stderr) {
			child.stderr.on("data", (chunk) => {
				const text = chunk.toString("utf8");
				if (text) {
					emit({ kind: "event", event: { type: "stderr", text } });
				}
			});
		}

		const bridgeOutput = createInterface({ input: child.stdout, crlfDelay: Infinity });
		bridgeOutput.on("line", (line) => {
			const trimmed = line.trim();
			if (!trimmed) return;

			let message;
			try {
				message = JSON.parse(trimmed);
			} catch {
				return;
			}

			// Translate bridge NDJSON to kernel WorkerEvents.
			// The bridge emits: ready, busy, idle, stream, display, result, error, reply, exit
			// We prefix with "kernel_" and wrap in { kind: "event", event: ... }
			switch (message.kind) {
				case "ready":
					emit({
						kind: "event",
						event: { type: "kernel_ready", kernel: message.kernel ?? kernel },
					});
					if (!bootstrapped && bootstrapCode) {
						bootstrapped = true;
						sendExec("bootstrap-0", bootstrapCode);
					}
					return;

				case "busy":
					emit({
						kind: "event",
						event: { type: "kernel_busy", input_id: message.input_id },
					});
					return;

				case "idle":
					emit({
						kind: "event",
						event: { type: "kernel_idle", input_id: message.input_id },
					});
					return;

				case "stream":
					emit({
						kind: "event",
						event: {
							type: "kernel_stream",
							input_id: message.input_id,
							name: message.name ?? "stdout",
							text: message.text ?? "",
						},
					});
					return;

				case "display":
					emit({
						kind: "event",
						event: {
							type: "kernel_display",
							input_id: message.input_id,
							data: message.data ?? {},
							metadata: message.metadata,
						},
					});
					return;

				case "result":
					emit({
						kind: "event",
						event: {
							type: "kernel_result",
							input_id: message.input_id,
							data: message.data ?? {},
							metadata: message.metadata,
							execution_count: message.execution_count ?? 0,
						},
					});
					return;

				case "error":
					emit({
						kind: "event",
						event: {
							type: "kernel_error",
							input_id: message.input_id,
							ename: message.ename ?? "",
							evalue: message.evalue ?? "",
							traceback: message.traceback ?? [],
						},
					});
					return;

				case "reply":
					emit({
						kind: "event",
						event: {
							type: "kernel_reply",
							input_id: message.input_id,
							status: message.status ?? "error",
							execution_count: message.execution_count ?? 0,
						},
					});
					return;

				case "host_request":
					emit({
						kind: "event",
							event: {
								type: "kernel_host_request",
								request_id: message.request_id,
								input_id: message.input_id,
								payload: message.payload ?? {},
						},
					});
					return;

				case "exit":
					emit({
						kind: "event",
						event: {
							type: "kernel_exit",
							kernel: message.kernel ?? kernel,
							exit_code: message.exit_code ?? null,
							signal: null,
						},
					});
					return;
			}
		});

		const unsubscribe = channel.subscribe((msg) => {
			if (closed) return;

			if (msg.kind === "kernel_execute" && typeof msg.code === "string") {
				sendExec(msg.input_id || "kernel", msg.code);
				return;
			}

			if (msg.kind === "kernel_interrupt") {
				if (child.pid) {
					// Send SIGINT to the bridge process group — the bridge will
					// forward it to the kernel via km.interrupt_kernel()
					child.stdin.write(`${JSON.stringify({ kind: "interrupt" })}\n`);
				}
				return;
			}

			if (msg.kind === "kernel_host_response" && typeof msg.request_id === "string") {
				child.stdin.write(`${JSON.stringify({
					kind: "host_response",
					request_id: msg.request_id,
					ok: msg.ok === true,
					result: msg.result,
					error: msg.error,
				})}\n`);
				return;
			}

			if (msg.kind === "kernel_close") {
				closed = true;
				if (child.stdin?.destroyed) {
					child.kill("SIGTERM");
					return;
				}
				child.stdin.write(`${JSON.stringify({ kind: "close" })}\n`);
				child.stdin.end();
			}
		});

		const unsubscribeClose = channel.onClose(() => {
			if (closed) return;
			closed = true;
			child.kill("SIGTERM");
		});

		child.once("close", (code, signal) => {
			unsubscribe();
			unsubscribeClose();
			bridgeOutput.close();
			const exitCode = typeof code === "number" ? code : 1;
			emit({
				kind: "event",
				event: {
					type: "kernel_exit",
					kernel,
					exit_code: code,
					signal,
				},
			});
			emit({
				kind: "result",
				result: {
					status: exitCode === 0 ? "done" : "error",
					mode: "kernel",
					kernel,
					exit_code: exitCode,
					signal,
				},
			});
			resolve(exitCode);
		});
	});
}

// ─── Main ────────────────────────────────────────────────────────────────

async function main() {
	const channel = createControlChannel();
	const config = await channel.readInit();
	if (config.mode === "kernel") {
		return runKernelWorkload(config, channel);
	}
	if (config.mode === "runtime") {
		return runRuntimeWorkload(config, channel);
	}
	if (config.mode === "command" || Array.isArray(config.command)) {
		return runCommandWorkload(config);
	}
	return runLoopWorkload(config);
}

main()
	.then((exitCode) => process.exit(exitCode ?? 0))
	.catch((err) => {
		emit({
			kind: "result",
			result: { status: "error", error: err.message },
		});
		process.exit(1);
	});
