export default async function Page() {
    let result = "";
    try {
        const response = await fetch(`${process.env.API_BASE_URL}/health`, { cache: "no-store" });
        if (response.ok) {
            const data = await response.json();
            result = JSON.stringify(data, null, 2);

        } else {
            result = `Error: ${response.status} ${response.statusText}`;
        }
    } catch (error: any) {
        result = `Error: ${error.message}`;
    }

    return (
        <div>

            <h1 className="text-nanobank-blue-deep font-bold">API Health Check</h1>
            <pre>{result}</pre>
        </div>
    );
}
