import pytest


@pytest.fixture(autouse=True)
def _no_claude_config_dir(monkeypatch: pytest.MonkeyPatch) -> None:
    # Tests point HOME at a temporary directory; a CLAUDE_CONFIG_DIR inherited
    # from the developer's shell would send them to the real one instead.
    monkeypatch.delenv("CLAUDE_CONFIG_DIR", raising=False)
