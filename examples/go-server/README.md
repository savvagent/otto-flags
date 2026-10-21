# Otto Flags Go Server Example

Example Go application demonstrating how to use the Otto Flags Go Server SDK with Gin framework.

## Features

- Go 1.21+
- Gin web framework
- Otto Flags Go Server SDK
- RESTful API endpoints
- Feature-gated functionality
- Goroutine-safe concurrent access
- In-memory caching

## Prerequisites

- Go 1.21 or higher

## Setup

1. **Install dependencies:**
   ```bash
   go mod download
   ```

   The example's `go.mod` has a `replace` directive that points the SDK import at
   `../../packages/go-server`, so it builds against the checkout without a published version.

2. **Configure environment variables:**
   ```bash
   export OTTO_FLAGS_API_URL=http://localhost:8080
   export OTTO_FLAGS_SDK_KEY=your-sdk-key-here
   export PORT=8082
   ```

3. **Run the server:**
   ```bash
   go run main.go
   ```

4. The server will start on [http://localhost:8082](http://localhost:8082)

## API Endpoints

### Health Check

```bash
GET /health
```

Returns server status.

### Get User Features

```bash
GET /api/features?userId=user-123
```

Returns all feature flags for a user.

Example response:
```json
{
  "userId": "user-123",
  "features": {
    "newUI": true,
    "betaFeatures": false,
    "advancedAnalytics": true
  }
}
```

### Process Data (Feature-Gated)

```bash
POST /api/data
Content-Type: application/json

{
  "userId": "user-123",
  "data": "example data"
}
```

Uses the `advanced-processing` flag to determine processing method.

## Usage Example

```go
package main

import (
    "log"
    "os"

    "github.com/gin-gonic/gin"
    ottoflags "github.com/savvagent/otto-flags/packages/go-server"
)

func main() {
    client, err := ottoflags.NewClient(ottoflags.Config{
        APIKey:  os.Getenv("OTTO_FLAGS_SDK_KEY"),
        BaseURL: os.Getenv("OTTO_FLAGS_API_URL"),
    })
    if err != nil {
        log.Fatal(err)
    }
    defer client.Close()

    r := gin.Default()

    r.GET("/api/features", func(c *gin.Context) {
        userId := c.Query("userId")

        isEnabled := client.IsEnabled("new-feature", &ottoflags.Context{
            UserID: userId,
            Attributes: map[string]interface{}{
                "plan": "pro",
            },
        })

        c.JSON(200, gin.H{
            "userId":     userId,
            "newFeature": isEnabled,
        })
    })

    r.Run(":8082")
}
```

## Learn More

- [Go Documentation](https://go.dev/doc/)
- [Gin Framework](https://gin-gonic.com/)
- [Otto Flags Go SDK Documentation](../../packages/go-server/README.md)
- [SDK Integration Guide](../../docs/SDK-INTEGRATION.md)
