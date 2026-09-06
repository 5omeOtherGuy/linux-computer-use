"""Independent cleanup command, usable even after a stuck worker is killed."""
import sys
from .x11 import recover_inputs

if __name__ == "__main__":
    recover_inputs(sys.argv[1])
