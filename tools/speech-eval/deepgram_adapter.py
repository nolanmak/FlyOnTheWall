"""Explicit, version-pinned Nova-3 uploads for the paired raw-ASR evaluator."""
import argparse
from datetime import date
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import ssl
import sys
from urllib.parse import urlencode

import benchmark


def model_version(model_id):
    """Require an exact dated Nova-3 version; aliases cannot pin a comparison."""
    match = re.fullmatch(r'nova-3@(\d{4}-\d{2}-\d{2})\.([0-9]{1,12})', model_id)
    if match is None:
        raise ValueError('an explicit dated Nova-3 version is required')
    date.fromisoformat(match[1])
    return model_id.split('@', 1)[1]


def transcribe(audio_path, model_id, terms, credential, timeout_seconds=120):
    """Upload one validated WAV once; reject errors or a changed server model.

    HTTPS uses the system trust roots, no proxy discovery, redirects or retries.
    Run under the benchmark's owned process deadline for a total wall-time bound.
    """
    version = model_version(model_id)
    if (not isinstance(terms, list) or len(terms) > 100
            or any(not isinstance(t, str) or not t.strip() or len(t) > 200
                   or any(ord(c) < 32 or ord(c) == 127 for c in t) for t in terms)):
        raise ValueError('invalid keyterms')
    if (not isinstance(credential, str) or not 1 <= len(credential) <= 4096
            or any(not 33 <= ord(c) <= 126 for c in credential)):
        raise ValueError('missing or invalid credential')
    benchmark.finite(timeout_seconds)
    if not 0 < timeout_seconds <= 3600:
        raise ValueError('invalid request timeout')
    path = Path(audio_path)
    if not path.is_file() or path.stat().st_size > benchmark.MAX_AUDIO_BYTES:
        raise ValueError('invalid WAV size')
    benchmark.wav_duration(path)
    with path.open('rb') as audio:
        data = audio.read(benchmark.MAX_AUDIO_BYTES + 1)
    if len(data) > benchmark.MAX_AUDIO_BYTES:
        raise ValueError('WAV changed beyond size limit')
    query = [('model', 'nova-3'), ('version', version), ('language', 'en'),
             ('smart_format', 'false'), ('punctuate', 'false'), ('diarize', 'false'),
             ('utterances', 'false'), ('mip_opt_out', 'true')]
    query.extend(('keyterm', term) for term in terms)
    connection = http.client.HTTPSConnection('api.deepgram.com', timeout=timeout_seconds,
                                             context=ssl.create_default_context())
    try:
        connection.request('POST', '/v1/listen?' + urlencode(query), body=data,
                           headers={'Authorization': 'Token ' + credential,
                                    'Content-Type': 'audio/wav'})
        response = connection.getresponse()
        if response.status != 200:
            raise ValueError('transcription request failed')
        raw = response.read(benchmark.MAX_RESPONSE_BYTES + 1)
        if len(raw) > benchmark.MAX_RESPONSE_BYTES:
            raise ValueError('response exceeds byte limit')
    finally:
        connection.close()
    doc = benchmark.strict_json(raw)
    try:
        metadata = doc['metadata']
        models = metadata['models']
        if not isinstance(models, list) or len(models) != 1 or not isinstance(models[0], str):
            raise ValueError('expected one server model')
        info = metadata['model_info'][models[0]]
        if info['arch'] != 'nova-3' or info['version'] != version:
            raise ValueError('server model does not match requested pin')
        channels = doc['results']['channels']
        if not isinstance(channels, list) or len(channels) != 1:
            raise ValueError('expected one transcript channel')
        alternatives = channels[0]['alternatives']
        if not isinstance(alternatives, list) or len(alternatives) != 1:
            raise ValueError('expected one transcript alternative')
        text = alternatives[0]['transcript']
        if not isinstance(text, str) or len(text) > 200000:
            raise ValueError('invalid raw transcript')
    except (KeyError, TypeError, IndexError) as error:
        raise ValueError('invalid server response') from error
    return {'schema_version': 1, 'audio_sha256': hashlib.sha256(data).hexdigest(),
            'model_id': model_id, 'text': text}


def main(argv=None):
    """Emit only the adapter protocol; never expose provider errors or credentials."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--allow-cloud', action='store_true')
    parser.add_argument('--audio', required=True)
    parser.add_argument('--model-id', required=True)
    parser.add_argument('--terms-json', default='[]')
    args = parser.parse_args(argv)
    if not args.allow_cloud:
        print('deepgram evaluation failed: cloud opt-in required', file=sys.stderr)
        return 2
    try:
        result = transcribe(args.audio, args.model_id, benchmark.strict_json(args.terms_json),
                            os.environ.get('DEEPGRAM_API_KEY'))
    except (ValueError, OSError, http.client.HTTPException, UnicodeError):
        print('deepgram evaluation failed', file=sys.stderr)
        return 2
    print(json.dumps(result, ensure_ascii=True, separators=(',', ':')))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
