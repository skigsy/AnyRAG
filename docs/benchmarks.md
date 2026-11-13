# Benchmarks

The comparison below measures one AnyRAG feature under matched conditions. For setup, access control, recovery, and graph qualification, see [verification](evidence.md).

## Compact MCP evidence

**Compact MCP evidence used 31.7% fewer median input tokens, took 14.2% less median model response time, and produced 9 more fully correct cited answers out of 36.**

| Metric | Full MCP evidence | Compact MCP evidence |
| --- | ---: | ---: |
| Median model input tokens | 2,841.5 | 1,940.5 |
| Median model response time | 766 ms | 657 ms |
| Fully correct answers with exact citations | 24/36 | 33/36 |

The paired development test used 500 synthetic Markdown files and 12 frozen questions: six single-source, four two-source, and two unsupported. Both arms answered the same questions three times in alternating order. The model was Qwen3-4B Q4_K_M through llama.cpp b12207 on Windows Vulkan. Both arms used the same index, ordered passages, and answer contract. Graph and reranking were off.

The TypeScript MCP SDK 1.32.2 collected the results. The client forwarded one structured copy to the model in both arms. The compact passages kept their original source revisions, text, offsets, and full citation IDs. Three compact responses still miscopied an ID. The response time measures the model request; it excludes indexing, engine startup, and installation.

This measures compact versus full AnyRAG MCP payloads for one model and synthetic workload. It does not compare AnyRAG against raw file search, measure end-to-end time, or establish a general accuracy or cost gain.

[Enable compact MCP evidence](setup.md#search-a-personal-index-from-an-mcp-client)
