package main

// A content-addressed payload spool with group commit, as D4 and D47 describe.
//
// The committer releases the lock while it writes and calls fsync. That is the
// mechanism: other writers accumulate into the next group during the sync. A
// committer that holds the lock across the sync defeats group commit entirely,
// which a Rust prototype learned the hard way.

import (
	"crypto/sha256"
	"encoding/binary"
	"encoding/hex"
	"os"
	"sync"
	"time"
)

type spool struct {
	mu           sync.Mutex
	cv           *sync.Cond
	f            *os.File
	pending      []byte
	pendingCount int64
	writeOffset  int64
	durableUpto  int64
	committing   bool
	linger       time.Duration

	fsyncs   int64
	groupSum int64
	maxGroup int64
}

func newSpool(path string, linger time.Duration) (*spool, error) {
	f, err := os.OpenFile(path, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0o600)
	if err != nil {
		return nil, err
	}
	s := &spool{f: f, linger: linger}
	s.cv = sync.NewCond(&s.mu)
	return s, nil
}

// put appends a payload and returns its content address once the bytes are
// durable. The caller may then reference it from a Corndogs task.
func (s *spool) put(payload []byte) (string, error) {
	sum := sha256.Sum256(payload)
	ref := hex.EncodeToString(sum[:16])

	frame := make([]byte, 0, len(payload)+40)
	var hdr [8]byte
	binary.LittleEndian.PutUint32(hdr[0:4], uint32(len(payload)))
	frame = append(frame, hdr[:]...)
	frame = append(frame, sum[:]...)
	frame = append(frame, payload...)

	s.mu.Lock()
	s.pending = append(s.pending, frame...)
	s.pendingCount++
	myEnd := s.writeOffset + int64(len(s.pending))

	if s.committing {
		for s.durableUpto < myEnd {
			s.cv.Wait()
		}
		s.mu.Unlock()
		return ref, nil
	}

	s.committing = true
	if s.linger > 0 {
		s.mu.Unlock()
		time.Sleep(s.linger)
		s.mu.Lock()
	}

	for {
		buf := s.pending
		group := s.pendingCount
		s.pending = nil
		s.pendingCount = 0
		if len(buf) == 0 {
			s.committing = false
			s.cv.Broadcast()
			s.mu.Unlock()
			return ref, nil
		}
		at := s.writeOffset
		s.writeOffset += int64(len(buf))
		s.groupSum += group
		if group > s.maxGroup {
			s.maxGroup = group
		}

		// Release the lock across the expensive part.
		s.mu.Unlock()
		_, werr := s.f.WriteAt(buf, at)
		var serr error
		if werr == nil {
			serr = s.f.Sync()
		}
		s.mu.Lock()
		s.fsyncs++
		if end := at + int64(len(buf)); end > s.durableUpto {
			s.durableUpto = end
		}
		s.cv.Broadcast()
		if werr != nil || serr != nil {
			s.committing = false
			s.cv.Broadcast()
			s.mu.Unlock()
			if werr != nil {
				return "", werr
			}
			return "", serr
		}
		if s.durableUpto >= myEnd && len(s.pending) == 0 {
			s.committing = false
			s.cv.Broadcast()
			s.mu.Unlock()
			return ref, nil
		}
	}
}

func (s *spool) stats() (fsyncs int64, meanGroup float64, maxGroup int64) {
	s.mu.Lock()
	defer s.mu.Unlock()
	m := 0.0
	if s.fsyncs > 0 {
		m = float64(s.groupSum) / float64(s.fsyncs)
	}
	return s.fsyncs, m, s.maxGroup
}
