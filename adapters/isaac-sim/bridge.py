"""Authenticated, owner-thread-only Isaac Sim bridge (protocol v1).

The concrete adapter targets Isaac Sim 4.5's Python API.  Isaac is imported by
the operator, after ``SimulationApp`` starts; this module remains importable on
machines without Isaac for protocol tests.
"""

from __future__ import annotations
import base64, binascii, hmac, json, os, queue, struct, threading, time, zlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

VERSION = 1
MAX_BODY = 256 * 1024
MAX_QUEUE = 128
QUEUE_DEADLINE = 2.0
TOKEN = os.environ.get("SERVOLOOP_BRIDGE_TOKEN")


def _finite(value):
    if isinstance(value, bool):
        return False
    if isinstance(value, float) and not __import__("math").isfinite(value):
        return False
    if isinstance(value, dict):
        return all(isinstance(k, str) and _finite(v) for k, v in value.items())
    if isinstance(value, list):
        return all(_finite(v) for v in value)
    return True


def _png_1x1():
    raw = b"\x00\xff\x00\x00"

    def chunk(kind, data):
        return (
            struct.pack(">I", len(data))
            + kind
            + data
            + struct.pack(">I", binascii.crc32(kind + data) & 0xFFFFFFFF)
        )

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", 1, 1, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b"")
    )


def _png_bytes(width, height, rgb_rows):
    def chunk(kind, data):
        return (
            struct.pack(">I", len(data))
            + kind
            + data
            + struct.pack(">I", binascii.crc32(kind + data) & 0xFFFFFFFF)
        )

    header = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(rgb_rows))
        + chunk(b"IEND", b"")
    )


class MockBackend:
    def __init__(self):
        self.joints, self.episode, self.sim_time = {"shoulder": 0.0}, 0, 0.0
        self.running, self.stop_calls, self.commands = True, 0, {}
        self.lock = threading.Lock()

    def observe(self):
        return {
            "joints": self.joints.copy(),
            "battery_percent": None,
            "emergency_stop": not self.running,
            "metadata": {"frame": "world", "units": "radians"},
            "simulation_time_s": self.sim_time,
            "episode_id": self.episode,
        }

    def command(self, body):
        if not isinstance(body, dict) or set(body) != {
            "command_id",
            "episode_id",
            "command",
        }:
            raise ValueError("malformed command envelope")
        cid, episode, c = body["command_id"], body["episode_id"], body["command"]
        if (
            not isinstance(cid, str)
            or not cid
            or len(cid) > 128
            or not isinstance(episode, int)
            or isinstance(episode, bool)
        ):
            raise ValueError("invalid command identity")
        if episode != self.episode:
            raise ValueError("stale episode")
        payload = json.dumps(body["command"], sort_keys=True, separators=(",", ":"))
        if cid in self.commands:
            old, result = self.commands[cid]
            if old != payload:
                raise ValueError("command id payload mismatch")
            return result
        if len(self.commands) >= 1024:
            raise ValueError("command deduplication capacity exhausted")
        allowed = {
            "move_joint": {"command", "joint", "position"},
            "move_joints": {"command", "positions"},
            "stop": {"command"},
        }
        if (
            not isinstance(c, dict)
            or c.get("command") not in allowed
            or set(c) != allowed[c.get("command")]
        ):
            raise ValueError("malformed command")
        if c["command"] == "stop":
            self.request_stop()
            self.stop()
        elif c["command"] == "move_joint":
            self._joint(c["joint"], c["position"])
            self.joints[c["joint"]] = c["position"]
        else:
            if not isinstance(c["positions"], dict) or not c["positions"]:
                raise ValueError("invalid joint targets")
            for j, p in c["positions"].items():
                self._joint(j, p)
            self.joints.update(c["positions"])
        result = {
            "accepted": True,
            "outcome": "completed",
            "message": "command completed",
            "metadata": {"episode_id": self.episode},
        }
        self.commands[cid] = (payload, result)
        return result

    def _joint(self, j, p):
        if (
            not isinstance(j, str)
            or j not in self.joints
            or isinstance(p, bool)
            or not isinstance(p, (int, float))
            or not __import__("math").isfinite(p)
            or abs(p) > 3.2
        ):
            raise ValueError("invalid joint target")

    def stop(self):
        self.stop_calls += 1
        self.running = False

    def reset(self):
        self.joints = {"shoulder": 0.0}
        self.episode += 1
        self.sim_time = 0.0
        self.running = True
        return {"episode_id": self.episode}

    def step(self, n):
        if not self.running:
            return {"steps": 0, "simulation_time_s": self.sim_time}
        self.sim_time += n / 60
        return {"steps": n, "simulation_time_s": self.sim_time}

    def camera(self, name):
        if name != "front":
            raise ValueError("unknown camera")
        return {
            "camera": name,
            "encoding": "image/png",
            "width": 1,
            "height": 1,
            "data_base64": base64.b64encode(_png_1x1()).decode(),
            "simulation_time_s": self.sim_time,
            "wall_time_unix_ms": int(time.time() * 1000),
            "frame": "front_optical",
            "units": "meters",
        }


class IsaacBackend:
    """Operator-configured Isaac Sim 4.5 backend; every method is owner-thread-only.

    Pass an already initialized ``World``, an ``Articulation``, and camera
    objects keyed by name.  The documented 4.5 methods used are
    ``World.step(render=True)``, ``World.reset()``, articulation
    ``get_joint_positions``, ``ArticulationController.apply_action`` with an
    ``ArticulationAction``, and camera ``get_rgba``.  Stop pauses the World;
    this is a simulation pause, not a hardware safety guarantee.
    No USD paths or imports are accepted from HTTP clients.
    """

    def __init__(
        self,
        world,
        articulation,
        cameras,
        joint_names,
        controller=None,
        action_factory=None,
    ):
        if (
            not world
            or not articulation
            or not isinstance(cameras, dict)
            or not joint_names
        ):
            raise ValueError("incomplete Isaac operator configuration")
        (
            self.world,
            self.articulation,
            self.cameras,
            self.joint_names,
            self.controller,
        ) = world, articulation, cameras, tuple(joint_names), controller
        self.episode, self.sim_time, self.stopped = 0, 0.0, False
        self.commands = {}
        self.stop_requested = threading.Event()
        self.stop_lock = threading.Lock()
        self.stop_generation = 0
        self.acknowledged_generation = 0
        self.action_factory = action_factory
        for camera in cameras.values():
            if hasattr(camera, "initialize"):
                camera.initialize()

    def observe(self):
        values = self.articulation.get_joint_positions()
        return {
            "joints": dict(zip(self.joint_names, (float(x) for x in values))),
            "battery_percent": None,
            "emergency_stop": self.stopped,
            "metadata": {"backend": "isaac-sim-4.5", "units": "radians"},
            "simulation_time_s": self.sim_time,
            "episode_id": self.episode,
        }

    def command(self, body):
        if set(body) != {"command_id", "episode_id", "command"}:
            raise ValueError("malformed command envelope")
        cid = body["command_id"]
        if not isinstance(cid, str) or not cid or len(cid) > 128:
            raise ValueError("invalid command identity")
        if body["episode_id"] != self.episode:
            raise ValueError("stale episode")
        payload = json.dumps(body["command"], sort_keys=True, separators=(",", ":"))
        if cid in self.commands:
            old, result = self.commands[cid]
            if old != payload:
                raise ValueError("command id payload mismatch")
            return result
        if len(self.commands) >= 1024:
            raise ValueError("command deduplication capacity exhausted")
        c = body["command"]
        if not isinstance(c, dict) or set(c) not in (
            {"command"},
            {"command", "joint", "position"},
            {"command", "positions"},
        ):
            raise ValueError("malformed command")
        if self.stop_requested.is_set() and c["command"] != "stop":
            raise ValueError("backend is stopped; reset is required")
        if c["command"] == "stop":
            self.stop()
        elif c["command"] == "move_joint":
            self._set({c["joint"]: c["position"]})
        elif c["command"] == "move_joints":
            self._set(c["positions"])
        else:
            raise ValueError("unsupported command")
        result = {
            "accepted": True,
            "outcome": "completed",
            "message": "command completed",
            "metadata": {"episode_id": self.episode},
        }
        self.commands[cid] = (payload, result)
        return result

    def _set(self, positions):
        if not isinstance(positions, dict) or not positions:
            raise ValueError("invalid joint targets")
        if any(
            j not in self.joint_names
            or isinstance(p, bool)
            or not isinstance(p, (int, float))
            or not __import__("math").isfinite(p)
            or abs(p) > 3.2
            for j, p in positions.items()
        ):
            raise ValueError("invalid joint target")
        if self.controller is None or not hasattr(self.controller, "apply_action"):
            raise ValueError("an ArticulationController is required")
        # Import only inside the operator-created Isaac process.  Tests inject
        # action_factory and never need NVIDIA packages.
        factory = getattr(self, "action_factory", None)
        action_type = None
        if factory is None:
            import numpy as np
            from omni.isaac.core.utils.types import ArticulationAction

            factory = ArticulationAction
            action_type = ArticulationAction
        indices = [self.joint_names.index(j) for j in positions]
        if action_type is not None:
            action = factory(
                joint_positions=np.array([positions[j] for j in positions]),
                joint_indices=np.array(indices),
            )
        else:
            action = factory(
                joint_positions=[positions[j] for j in positions], joint_indices=indices
            )
        self.controller.apply_action(action)
        for _ in range(120):
            if self.stop_requested.is_set():
                break
            self.world.step(render=True)
            self.sim_time += 1 / 60
            actual = self.articulation.get_joint_positions()
            if all(
                abs(float(actual[i]) - float(positions[j])) <= 1e-3
                for j, i in zip(positions, indices)
            ):
                return
        actual = self.articulation.get_joint_positions()
        if not all(
            abs(float(actual[i]) - float(positions[j])) <= 1e-3
            for j, i in zip(positions, indices)
        ):
            raise ValueError("joint target did not converge within bounded steps")

    def request_stop(self):
        with self.stop_lock:
            self.stop_generation += 1
            self.stop_requested.set()

    def stop(self):
        with self.stop_lock:
            generation = self.stop_generation
            self.stop_requested.set()
        if hasattr(self.world, "pause"):
            self.world.pause()
        elif self.controller and hasattr(self.controller, "stop"):
            self.controller.stop()
        else:
            raise ValueError("Isaac World has no supported simulation pause")
        with self.stop_lock:
            if generation != self.stop_generation:
                return
            self.stopped = True
            self.acknowledged_generation = generation

    def reset(self):
        with self.stop_lock:
            generation = self.stop_generation
            if generation == 0 or self.acknowledged_generation != generation:
                raise ValueError("reset requires an acknowledged stop")
        self.world.reset()
        with self.stop_lock:
            if (
                generation != self.stop_generation
                or self.acknowledged_generation != generation
            ):
                self.stop_requested.set()
                self.stopped = True
                raise ValueError("stop changed during reset")
            self.stop_requested.clear()
            self.stopped = False
            self.episode += 1
            self.sim_time = 0.0
            return {"episode_id": self.episode}

    def step(self, n):
        done = 0
        for _ in range(n):
            if self.stopped or self.stop_requested.is_set():
                break
            self.world.step(render=True)
            self.sim_time += 1 / 60
            done += 1
        return {"steps": done, "simulation_time_s": self.sim_time}

    def camera(self, name):
        if name not in self.cameras:
            raise ValueError("unknown camera")
        camera = self.cameras[name]
        rgba = camera.get_rgba()
        try:
            height, width = int(rgba.shape[0]), int(rgba.shape[1])
        except (AttributeError, IndexError, TypeError, ValueError):
            raise ValueError("camera returned invalid dimensions")
        if width <= 0 or height <= 0 or width > MAX_BODY // 3 or height > MAX_BODY // 3:
            raise ValueError("camera dimensions exceed size limit")
        raw_size = height * (1 + width * 3)
        if raw_size > MAX_BODY:
            raise ValueError("camera image exceeds size limit")
        rows = b"".join(
            b"\x00" + b"".join(bytes(pixel[:3]) for pixel in row) for row in rgba
        )
        png = _png_bytes(int(width), int(height), rows)
        if len(png) > MAX_BODY:
            raise ValueError("camera image exceeds size limit")
        return {
            "camera": name,
            "encoding": "image/png",
            "width": int(width),
            "height": int(height),
            "data_base64": base64.b64encode(png).decode(),
            "simulation_time_s": self.sim_time,
            "wall_time_unix_ms": int(time.time() * 1000),
            "frame": "camera_optical",
            "units": "meters",
        }


class Bridge:
    def __init__(self, backend):
        self.backend, self.work, self.sequence = (
            backend,
            queue.PriorityQueue(MAX_QUEUE),
            0,
        )

    def run_owner(self):
        while True:
            item = self.work.get()
            if item is None:
                return
            priority, deadline, _, fn, done = item
            if time.monotonic() > deadline:
                done.put((False, "work deadline exceeded"))
                continue
            try:
                done.put((True, fn()))
            except Exception as e:
                done.put((False, str(e)))

    def call(self, fn, priority=10, timeout=QUEUE_DEADLINE):
        done = queue.Queue(1)
        self.sequence += 1
        try:
            self.work.put_nowait(
                (priority, time.monotonic() + timeout, self.sequence, fn, done)
            )
        except queue.Full:
            raise ValueError("work queue full")
        try:
            ok, value = done.get(timeout=timeout + 1)
        except queue.Empty:
            raise ValueError("work deadline exceeded")
        if not ok:
            raise ValueError(value)
        return value


class Handler(BaseHTTPRequestHandler):
    bridge = None

    def send_json(self, status, body):
        raw = json.dumps(body, separators=(",", ":"), allow_nan=False).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def auth(self):
        token = os.environ.get("SERVOLOOP_BRIDGE_TOKEN")
        supplied = self.headers.get("Authorization", "")
        return bool(token) and hmac.compare_digest(
            supplied.encode(), ("Bearer " + token).encode()
        )

    def do_GET(self):
        if not self.auth():
            return self.send_json(401, {"error": "unauthorized"})
        try:
            if self.path == "/v1/observation":
                body = self.bridge.call(self.bridge.backend.observe)
            elif self.path.startswith("/v1/cameras/") and self.path.count("/") == 3:
                body = self.bridge.call(
                    lambda: self.bridge.backend.camera(self.path.rsplit("/", 1)[-1])
                )
            else:
                return self.send_json(404, {"error": "not_found"})
            self.send_json(200, body)
        except ValueError as e:
            self.send_json(400, {"error": str(e)})

    def _body(self):
        try:
            length = int(self.headers.get("Content-Length", "-1"))
        except ValueError:
            raise ValueError("invalid content length")
        if length < 0 or length > MAX_BODY:
            raise OverflowError
        chunks = []
        remaining = length
        while remaining:
            chunk = self.rfile.read(min(8192, remaining))
            if not chunk:
                raise ValueError("truncated body")
            chunks.append(chunk)
            remaining -= len(chunk)
        envelope = json.loads(b"".join(chunks))
        if (
            not isinstance(envelope, dict)
            or set(envelope) - {"schema_version", "request_id", "body"}
            or not isinstance(envelope.get("body", {}), dict)
            or not _finite(envelope)
        ):
            raise ValueError("malformed envelope")
        return envelope

    def do_POST(self):
        if not self.auth():
            return self.send_json(401, {"error": "unauthorized"})
        try:
            envelope = self._body()
        except OverflowError:
            return self.send_json(413, {"error": "body_too_large"})
        except Exception:
            return self.send_json(400, {"error": "malformed_json"})
        if envelope.get("schema_version") != VERSION:
            return self.send_json(426, {"error": "unsupported_protocol"})
        try:
            if self.path == "/v1/handshake":
                if set(envelope["body"]) not in (
                    set(),
                    {"min_version", "max_version", "scene"},
                ):
                    raise ValueError("unknown handshake field")
                body = {
                    "min_version": VERSION,
                    "max_version": VERSION,
                    "scene": "operator-configured",
                }
            elif self.path == "/v1/commands":
                body = self.bridge.call(
                    lambda: self.bridge.backend.command(envelope["body"])
                )
            elif self.path == "/v1/episode/reset":
                if envelope["body"]:
                    raise ValueError("reset body must be empty")
                body = self.bridge.call(self.bridge.backend.reset)
            elif self.path == "/v1/simulation/step":
                n = envelope["body"]["steps"]
                if not isinstance(n, int) or isinstance(n, bool) or not 1 <= n <= 1000:
                    raise ValueError("step count must be 1..=1000")
                body = self.bridge.call(lambda: self.bridge.backend.step(n))
            elif self.path == "/v1/stop":
                if envelope["body"]:
                    raise ValueError("stop body must be empty")
                if hasattr(self.bridge.backend, "request_stop"):
                    self.bridge.backend.request_stop()

                def stop_ack():
                    self.bridge.backend.stop()
                    return {"stopped": True}

                body = self.bridge.call(stop_ack, priority=0)
            else:
                return self.send_json(404, {"error": "not_found"})
            self.send_json(200, body)
        except (KeyError, TypeError, ValueError):
            self.send_json(400, {"error": "invalid_request"})

    def log_message(self, *_):
        pass


def serve(host="127.0.0.1", port=8765):
    if host not in ("127.0.0.1", "localhost", "::1"):
        raise ValueError("bridge only binds loopback; use a TLS reverse proxy")
    if not os.environ.get("SERVOLOOP_BRIDGE_TOKEN"):
        raise RuntimeError("SERVOLOOP_BRIDGE_TOKEN must be set")
    serve_backend(MockBackend(), host, port)


def serve_backend(backend, host="127.0.0.1", port=8765):
    """Run HTTP workers and the backend owner loop for an operator backend.

    Construct ``SimulationApp``, ``World``, and ``IsaacBackend`` first, then
    call this function from the simulator process.  The owner loop is the only
    thread that invokes World, articulation, controller, or camera methods.
    """
    if host not in ("127.0.0.1", "localhost", "::1"):
        raise ValueError("bridge only binds loopback; use a TLS reverse proxy")
    if not os.environ.get("SERVOLOOP_BRIDGE_TOKEN"):
        raise RuntimeError("SERVOLOOP_BRIDGE_TOKEN must be set")
    bridge = Bridge(backend)
    Handler.bridge = bridge
    server = ThreadingHTTPServer((host, port), Handler)
    http_thread = threading.Thread(target=server.serve_forever, daemon=True)
    http_thread.start()
    try:
        # Isaac World ownership stays on the caller's simulator thread.
        bridge.run_owner()
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    serve(
        os.environ.get("SERVOLOOP_BRIDGE_HOST", "127.0.0.1"),
        int(os.environ.get("SERVOLOOP_BRIDGE_PORT", "8765")),
    )
