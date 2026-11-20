"""The trace a simulation prints: one line for each thing the worker does."""


class Trace:
    def __init__(self):
        self.lines = []
        self.runs = 0
        self.ok = 0
        self.gave_up = 0

    def run(self, moment, attempt, outcome):
        self.runs += 1
        self.lines.append(f"t={moment} run {attempt.job.name} attempt {attempt.number} -> {outcome}")

    def succeeded(self):
        self.ok += 1

    def give_up(self, moment, attempt):
        self.gave_up += 1
        self.lines.append(f"t={moment} give up {attempt.job.name} after {attempt.number} attempts")

    def finish(self, moment):
        self.lines.append(f"done at t={moment}: {self.ok} ok, {self.gave_up} gave up, {self.runs} runs")

    def text(self):
        return "\n".join(self.lines) + "\n"
