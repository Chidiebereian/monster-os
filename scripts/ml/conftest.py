import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))


def pytest_configure(config):
    config.addinivalue_line("markers", "slow: runs a research-mode backtest batch against projection-service (about 20 s)")
