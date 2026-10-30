package main

import (
	"fmt"
	"example.com/goapp/internal/store"
)

func main() {
	s := store.New()
	fmt.Println(s.Get("x"))
}
