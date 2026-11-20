import unittest

from deployctl import rules


def base():
    return {
        "service": {"name": "demo-api", "image": "r.example.internal/demo/api:1.0.3"},
        "deploy": {"env": "prod", "replicas": "3"},
        "http": {"port": "8000", "admin_port": "9000"},
        "health": {"path": "/healthz", "interval": "10s"},
    }


def messages(values):
    return [m for _, _, m in rules.run(values)]


class Rules(unittest.TestCase):
    def test_clean(self):
        self.assertEqual(messages(base()), [])

    def test_required(self):
        values = base()
        del values["deploy"]["replicas"]
        self.assertEqual(messages(values), ["[deploy] needs replicas"])

    def test_replicas(self):
        values = base()
        values["deploy"]["replicas"] = "1"
        self.assertEqual(messages(values), ["prod needs at least 2 replicas"])
        values["deploy"]["replicas"] = "51"
        self.assertEqual(len(messages(values)), 1)

    def test_ports(self):
        values = base()
        values["http"]["admin_port"] = "70000"
        self.assertEqual(messages(values), ["admin_port must be a port from 1 to 65535, not '70000'"])

    def test_image(self):
        values = base()
        values["service"]["image"] = "r.example.internal/demo/api:latest"
        self.assertEqual(messages(values), ["prod may not deploy the latest tag"])
        values["service"]["image"] = "r.example.internal:5000/demo/api"
        self.assertEqual(messages(values), ["image 'r.example.internal:5000/demo/api' has no tag"])

    def test_health(self):
        values = base()
        values["health"] = {"path": "healthz", "timeout": "soon"}
        self.assertEqual(len(messages(values)), 2)


if __name__ == "__main__":
    unittest.main()
