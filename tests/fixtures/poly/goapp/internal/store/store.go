package store

type Reader interface { Get(k string) string }

// Store keeps values in memory.
type Store struct { data map[string]string }

func New() *Store { return &Store{data: map[string]string{}} }

func (s *Store) Get(k string) string { return s.data[k] }
