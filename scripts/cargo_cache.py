#!/usr/bin/env python3
"""TWO-only Cargo admission, read-only retention audit, and filesystem alarm.

No deletion, mounting, credential access, or service control. Linux + stdlib only.
A filesystem quota is the hard byte bound; the sampled guard is defense in depth.
"""

import argparse
import fcntl
import json
import os
from pathlib import Path
import shutil
import signal
import stat
import subprocess
import sys
import time

GIB = 1024 ** 3
DEFAULT_POOL = Path('/paperclip/.cache/two-bot-next-bounded')
TERMINAL = {'done', 'cancelled'}


class Refusal(Exception):
    pass
