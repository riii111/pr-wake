package main

import (
	"context"
	"errors"
	"fmt"
	"os"
	"os/signal"
	"syscall"

	"github.com/riii111/pr-wake/internal/review"
	"github.com/riii111/pr-wake/internal/watch"
)

func main() {
	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()
	if err := run(ctx, os.Args[1:]); err != nil {
		fmt.Fprintln(os.Stderr, "pr-wake:", err)
		os.Exit(1)
	}
}

func run(ctx context.Context, args []string) error {
	if len(args) == 0 {
		return errors.Join(review.ErrUsage, watch.ErrUsage)
	}
	switch args[0] {
	case "review":
		return review.Run(ctx, args[1:], os.Stdout)
	case "watch":
		return watch.Run(args[1:], os.Stdout)
	default:
		return errors.Join(review.ErrUsage, watch.ErrUsage)
	}
}
