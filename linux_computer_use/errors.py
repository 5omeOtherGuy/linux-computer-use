class DesktopError(RuntimeError):
    """An actionable, recoverable desktop failure."""

    def __init__(self, code, message):
        self.code = code
        super().__init__(message)
