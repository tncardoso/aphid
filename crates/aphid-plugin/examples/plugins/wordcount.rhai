//! Adds a tool that counts the words in a file.

register_tool(#{
    name: "wordcount",
    description: "Count the words and lines in a file in the workspace.",
    parameters: #{
        type: "object",
        properties: #{
            path: #{ type: "string", description: "Path to the file, relative to the workspace." }
        },
        required: ["path"]
    },
    execute: |args| {
        let text = fs_read(args.path);
        #{
            content: text.split(' ').len() + " words, " + text.split('\n').len() + " lines",
            details: #{ path: args.path, bytes: text.len() }
        }
    }
});
