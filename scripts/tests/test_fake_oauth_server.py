import base64
import hashlib
import json
import pathlib
import sys
import threading
import unittest
import urllib.error
import urllib.parse
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts" / "test-support"))

import fake_oauth_server  # noqa: E402


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


OPENER = urllib.request.build_opener(NoRedirect)


def call(url, data=None, headers=None, method=None):
    request = urllib.request.Request(url, data=data, headers=headers or {}, method=method)
    try:
        with OPENER.open(request, timeout=5) as response:
            return response.status, dict(response.headers), response.read()
    except urllib.error.HTTPError as error:
        return error.code, dict(error.headers), error.read()


class FakeOAuthServerTests(unittest.TestCase):
    def setUp(self):
        self.server, self.state = fake_oauth_server.serve("127.0.0.1", 0)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        host, port = self.server.server_address
        self.origin = f"http://{host}:{port}"

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()

    def login(self, verifier="v" * 43, redirect="http://localhost:9/callback"):
        challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).decode().rstrip("=")
        query = urllib.parse.urlencode({
            "response_type": "code", "client_id": "client", "redirect_uri": redirect,
            "scope": "user:profile user:inference", "state": "st",
            "code_challenge": challenge, "code_challenge_method": "S256",
        })
        status, headers, _ = call(f"{self.origin}/oauth/authorize?{query}")
        self.assertEqual(status, 302)
        location = urllib.parse.urlsplit(headers["Location"])
        params = dict(urllib.parse.parse_qsl(location.query))
        self.assertEqual(params["state"], "st")
        body = json.dumps({"grant_type": "authorization_code", "code": params["code"], "state": "st",
                           "client_id": "client", "redirect_uri": redirect,
                           "code_verifier": verifier}).encode()
        return call(f"{self.origin}/oauth/token", body, {"Content-Type": "application/json"})

    def profile(self, token):
        return call(f"{self.origin}/api/oauth/profile", headers={"Authorization": f"Bearer {token}"})

    def test_login_binds_each_token_to_the_identity_at_authorize(self):
        status, _, body = self.login()
        self.assertEqual(status, 200)
        first = json.loads(body)
        self.assertTrue(first["access_token"].startswith("fake-"))
        self.assertEqual(first["scope"], "user:profile user:inference")
        call(f"{self.origin}/__control", json.dumps({"identity": {
            "account_uuid": "b", "organization_uuid": "o"}}).encode(),
            {"Content-Type": "application/json"})
        second = json.loads(self.login()[2])
        a = json.loads(self.profile(first["access_token"])[2])
        b = json.loads(self.profile(second["access_token"])[2])
        self.assertEqual(a["account"]["uuid"], "fake-account-a")
        self.assertEqual(b["account"]["uuid"], "b")
        self.assertNotIn("email", b["account"], "email is optional")

    def test_scripted_profile_statuses_then_success(self):
        token = json.loads(self.login()[2])["access_token"]
        call(f"{self.origin}/__control", json.dumps({"profile_statuses": [503, 429], "retry_after": 0}).encode(),
             {"Content-Type": "application/json"})
        self.assertEqual(self.profile(token)[0], 503)
        status, headers, _ = self.profile(token)
        self.assertEqual((status, headers.get("Retry-After")), (429, "0"))
        self.assertEqual(self.profile(token)[0], 200)
        self.assertEqual(self.profile("unknown")[0], 401)

    def test_pkce_and_redirect_are_enforced(self):
        rejected = call(f"{self.origin}/oauth/authorize?" + urllib.parse.urlencode({
            "response_type": "code", "client_id": "c", "redirect_uri": "https://evil.example/cb",
            "code_challenge": "x", "code_challenge_method": "S256"}))
        self.assertEqual(rejected[0], 400, "non-loopback redirect is refused")
        verifier = "w" * 43
        challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).decode().rstrip("=")
        query = urllib.parse.urlencode({
            "response_type": "code", "client_id": "client", "redirect_uri": "http://localhost:9/callback",
            "state": "s", "code_challenge": challenge, "code_challenge_method": "S256"})
        location = call(f"{self.origin}/oauth/authorize?{query}")[1]["Location"]
        code = dict(urllib.parse.parse_qsl(urllib.parse.urlsplit(location).query))["code"]
        bad = urllib.parse.urlencode({"grant_type": "authorization_code", "code": code, "client_id": "client",
                                      "redirect_uri": "http://localhost:9/callback",
                                      "code_verifier": "wrong"}).encode()
        self.assertEqual(call(f"{self.origin}/oauth/token", bad)[0], 400)

    def test_refresh_rotates_and_keeps_identity(self):
        first = json.loads(self.login()[2])
        body = json.dumps({"grant_type": "refresh_token", "refresh_token": first["refresh_token"],
                           "client_id": "client"}).encode()
        status, _, raw = call(f"{self.origin}/oauth/token", body, {"Content-Type": "application/json"})
        self.assertEqual(status, 200)
        rotated = json.loads(raw)
        self.assertNotEqual(rotated["refresh_token"], first["refresh_token"])
        self.assertEqual(json.loads(self.profile(rotated["access_token"])[2])["account"]["uuid"], "fake-account-a")
        self.assertEqual(call(f"{self.origin}/oauth/token", body, {"Content-Type": "application/json"})[0], 400)

    def test_log_never_records_tokens(self):
        token = json.loads(self.login()[2])["access_token"]
        self.profile(token)
        log = call(f"{self.origin}/__log")[2].decode()
        self.assertNotIn(token, log)
        self.assertNotIn("fake-rt-", log)

    def test_non_loopback_bind_is_refused(self):
        with self.assertRaises(SystemExit):
            fake_oauth_server.serve("0.0.0.0", 0)


if __name__ == "__main__":
    unittest.main()
