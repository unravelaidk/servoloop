import base64, json, os, struct, threading, unittest, zlib
from http.client import HTTPConnection
from bridge import Bridge, Handler, MockBackend, IsaacBackend, VERSION
from http.server import ThreadingHTTPServer


class BridgeTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        os.environ["SERVOLOOP_BRIDGE_TOKEN"] = "test-token"
        cls.backend = MockBackend()
        bridge = Bridge(cls.backend)
        Handler.bridge = bridge
        threading.Thread(target=bridge.run_owner, daemon=True).start()
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()
        cls.port = cls.server.server_port

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()

    def request(self, path, body=None, token="test-token"):
        c = HTTPConnection("127.0.0.1", self.port)
        raw = json.dumps({"schema_version": VERSION, "body": body or {}})
        c.request(
            "POST",
            path,
            raw,
            {"Authorization": "Bearer " + token, "Content-Type": "application/json"},
        )
        r = c.getresponse()
        return r.status, json.loads(r.read())

    def get(self, path):
        c = HTTPConnection("127.0.0.1", self.port)
        c.request("GET", path, headers={"Authorization": "Bearer test-token"})
        r = c.getresponse()
        return r.status, json.loads(r.read())

    def test_auth_and_version(self):
        self.assertEqual(self.request("/v1/handshake", token="wrong")[0], 401)
        self.assertEqual(self.request("/v1/handshake")[1]["max_version"], VERSION)
        c = HTTPConnection("127.0.0.1", self.port)
        c.request(
            "POST",
            "/v1/handshake",
            json.dumps({"schema_version": VERSION + 1, "body": {}}),
            {"Authorization": "Bearer test-token"},
        )
        self.assertEqual(c.getresponse().status, 426)

    def test_command_reset_and_bad_step(self):
        status, result = self.request(
            "/v1/commands",
            {
                "command_id": "one",
                "episode_id": 0,
                "command": {
                    "command": "move_joint",
                    "joint": "shoulder",
                    "position": 0.2,
                },
            },
        )
        self.assertEqual((status, result["outcome"]), (200, "completed"))
        self.assertEqual(self.request("/v1/simulation/step", {"steps": 0})[0], 400)
        self.assertEqual(self.request("/v1/episode/reset")[1]["episode_id"], 1)
        self.assertEqual(
            self.request(
                "/v1/commands",
                {
                    "command_id": "bad",
                    "episode_id": 1,
                    "command": {
                        "command": "move_joint",
                        "joint": "shoulder",
                        "position": 0.1,
                        "extra": 1,
                    },
                },
            )[0],
            400,
        )

    def test_stop_calls_backend_and_acknowledges(self):
        before = self.backend.stop_calls
        status, result = self.request("/v1/stop")
        self.assertEqual((status, result), (200, {"stopped": True}))
        self.assertEqual(self.backend.stop_calls, before + 1)

    def test_camera_is_valid_png(self):
        status, result = self.get("/v1/cameras/front")
        self.assertEqual(status, 200)
        self.assertEqual(result["data_base64"][:12], "iVBORw0KGgoA")

    def test_stale_episode_rejected(self):
        status, _ = self.request(
            "/v1/commands",
            {"command_id": "stale", "episode_id": 0, "command": {"command": "stop"}},
        )
        self.assertEqual(status, 400)

    def test_replay_mismatch_rejected(self):
        body = {
            "command_id": "replay",
            "episode_id": 1,
            "command": {"command": "move_joint", "joint": "shoulder", "position": 0.1},
        }
        self.assertEqual(self.request("/v1/commands", body)[0], 200)
        body["command"]["position"] = 0.2
        self.assertEqual(self.request("/v1/commands", body)[0], 400)

    def test_unknown_envelope_field_rejected(self):
        c = HTTPConnection("127.0.0.1", self.port)
        c.request(
            "POST",
            "/v1/handshake",
            json.dumps({"schema_version": VERSION, "body": {}, "extra": 1}),
            {"Authorization": "Bearer test-token"},
        )
        self.assertEqual(c.getresponse().status, 400)

    def test_remote_bind_rejected_even_with_tls_flag(self):
        from bridge import serve

        os.environ["SERVOLOOP_BRIDGE_TLS"] = "1"
        with self.assertRaises(ValueError):
            serve("0.0.0.0", 0)

    def test_nonfinite_json_rejected(self):
        c = HTTPConnection("127.0.0.1", self.port)
        c.request(
            "POST",
            "/v1/handshake",
            '{"schema_version":1,"body":{"x":NaN}}',
            {"Authorization": "Bearer test-token"},
        )
        self.assertEqual(c.getresponse().status, 400)


class FakeIsaacTest(unittest.TestCase):
    class World:
        def __init__(self):
            self.steps = 0
            self.paused = False

        def step(self, render=True):
            self.steps += 1

        def pause(self):
            self.paused = True

        def reset(self):
            self.steps = 0
            self.paused = False

    class Articulation:
        def __init__(self):
            self.positions = [0.0]
            self.controller = None

        def get_joint_positions(self):
            return self.positions

    class Controller:
        def __init__(self, articulation):
            self.articulation = articulation
            self.actions = 0

        def apply_action(self, action):
            self.actions += 1
            self.articulation.positions[:] = action["positions"]

    class Camera:
        class Pixels(list):
            shape = (2, 2, 4)

        def initialize(self):
            pass

        def get_rgba(self):
            return self.Pixels(
                [
                    [(255, 0, 0, 255), (0, 255, 0, 255)],
                    [(0, 0, 255, 255), (255, 255, 255, 255)],
                ]
            )

    def backend(self):
        world = self.World()
        articulation = self.Articulation()
        controller = self.Controller(articulation)
        return (
            IsaacBackend(
                world,
                articulation,
                {},
                ["shoulder"],
                controller,
                action_factory=lambda joint_positions, joint_indices: {
                    "positions": joint_positions
                },
            ),
            world,
            articulation,
            controller,
        )

    def test_controller_target_converges_without_teleport_api(self):
        backend, world, articulation, controller = self.backend()
        result = backend.command(
            {
                "command_id": "fake-1",
                "episode_id": 0,
                "command": {
                    "command": "move_joint",
                    "joint": "shoulder",
                    "position": 0.5,
                },
            }
        )
        self.assertEqual(result["outcome"], "completed")
        self.assertEqual(controller.actions, 1)
        self.assertEqual(articulation.positions, [0.5])

    def test_stop_pauses_world_and_blocks_new_dispatch(self):
        backend, world, articulation, controller = self.backend()
        backend.request_stop()
        backend.stop()
        self.assertTrue(world.paused)
        with self.assertRaises(ValueError):
            backend.command(
                {
                    "command_id": "fake-2",
                    "episode_id": 0,
                    "command": {
                        "command": "move_joint",
                        "joint": "shoulder",
                        "position": 0.5,
                    },
                }
            )
        self.assertEqual(controller.actions, 0)

    def test_dedup_capacity_rejects_before_side_effect(self):
        backend, world, articulation, controller = self.backend()
        backend.commands = {str(i): ("payload", {}) for i in range(1024)}
        with self.assertRaisesRegex(ValueError, "capacity"):
            backend.command(
                {
                    "command_id": "new",
                    "episode_id": 0,
                    "command": {
                        "command": "move_joint",
                        "joint": "shoulder",
                        "position": 0.5,
                    },
                }
            )
        self.assertEqual(controller.actions, 0)
        self.assertEqual(articulation.positions, [0.0])

    def test_two_by_two_camera_has_row_filters(self):
        backend, _, _, _ = self.backend()
        backend.cameras = {"front": self.Camera()}
        frame = backend.camera("front")
        encoded = base64.b64decode(frame["data_base64"])
        position = 8
        compressed = None
        while position < len(encoded):
            size = struct.unpack(">I", encoded[position : position + 4])[0]
            kind = encoded[position + 4 : position + 8]
            data = encoded[position + 8 : position + 8 + size]
            if kind == b"IDAT":
                compressed = (compressed or b"") + data
            position += 12 + size
        self.assertEqual(
            zlib.decompress(compressed),
            bytes([0, 255, 0, 0, 0, 255, 0, 0, 0, 0, 255, 255, 255, 255]),
        )

    def test_failed_pause_does_not_acknowledge_reset(self):
        backend, world, _, _ = self.backend()
        world.pause = lambda: (_ for _ in ()).throw(RuntimeError("pause failed"))
        backend.request_stop()
        with self.assertRaises(RuntimeError):
            backend.stop()
        with self.assertRaisesRegex(ValueError, "acknowledged"):
            backend.reset()

    def test_new_stop_during_reset_is_not_cleared(self):
        backend, world, _, _ = self.backend()
        backend.request_stop()
        backend.stop()
        original_reset = world.reset
        world.reset = lambda: (original_reset(), backend.request_stop())
        with self.assertRaisesRegex(ValueError, "changed"):
            backend.reset()
        self.assertTrue(backend.stop_requested.is_set())


if __name__ == "__main__":
    unittest.main()
