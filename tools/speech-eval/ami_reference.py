"""All-speaker AMI meeting reference (stub)."""


def channel_agents(root, meeting):
    return {}


def build_reference(root, meeting, mic_channel, window_s, duration_s=None):
    return {"reference": "", "words": [], "speaker_segments": [], "mic_speakers": [], "system_speakers": [],
            "duration_ms": 0, "mic_reference": "", "system_reference": "", "speaker_segment_source": ""}


def concatenate(docs, total_ms):
    return {"reference": "", "words": [], "speaker_segments": [], "duration_ms": 0}
