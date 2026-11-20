"""Synthetic notes shared by the tests."""

from notes.model import Note


def sample_notes():
    return [
        Note(1, "Buy paint", "white, two litres", ["home"], "2026-03-01T09:00:00Z"),
        Note(2, "Call the plumber", "", ["home", "urgent"], "2026-03-02T17:45:00Z"),
        Note(3, "Draft the quarterly plan", "milestones and owners", ["work"], "2026-03-02T08:00:00+02:00"),
        Note(4, "Read about tide tables", "", [], "2026-03-03T22:10:00Z"),
        Note(5, "Book the venue", "ask about the PLAN for parking", ["work", "Urgent"], "2026-03-04T06:30:00Z"),
    ]
