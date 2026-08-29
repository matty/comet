import importlib.util
import subprocess
import sys
import unittest
from pathlib import Path


SCRIPT_PATH = Path(__file__).parents[1] / "probe-acp-setters.py"
SPEC = importlib.util.spec_from_file_location("probe_acp_setters", SCRIPT_PATH)
probe = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(probe)


class CandidatesModelIdTests(unittest.TestCase):
    """D117: the probe's model id is a --model flag, not a hardcoded literal."""

    def test_default_model_id_is_the_placeholder(self):
        # No --model given: an argument-less run must behave exactly as it
        # did before the flag existed, i.e. still probe the placeholder.
        built = probe.candidates(probe.DEFAULT_MODEL_ID)
        self.assertEqual(built[0], ("session/set_model", {"modelId": "REPLACE_WITH_A_REAL_MODEL_ID"}))
        self.assertEqual(
            built[1],
            ("session/set_config_option", {"configId": "model", "value": "REPLACE_WITH_A_REAL_MODEL_ID"}),
        )

    def test_explicit_model_id_flows_into_the_model_setting_candidates(self):
        built = probe.candidates("grok-4-fast")
        self.assertEqual(built[0], ("session/set_model", {"modelId": "grok-4-fast"}))
        self.assertEqual(
            built[1],
            ("session/set_config_option", {"configId": "model", "value": "grok-4-fast"}),
        )
        # The mode candidates never carry a model id and must be untouched.
        self.assertEqual(built[2], ("session/set_mode", {"modeId": "low"}))
        self.assertEqual(built[3], ("session/set_mode", {"modeId": "not-a-real-mode-xyz"}))

    def test_help_documents_the_model_flag_with_a_real_example(self):
        result = subprocess.run(
            [sys.executable, str(SCRIPT_PATH), "--help"],
            capture_output=True,
            text=True,
            check=True,
        )
        self.assertIn("--model", result.stdout)
        self.assertIn("model id to probe", result.stdout)
        # A real value has to look nothing like the placeholder, else a
        # reader following --help would just paste the default back in.
        self.assertIn("grok-4-fast", result.stdout)


if __name__ == "__main__":
    unittest.main()
