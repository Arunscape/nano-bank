import Link from "next/link";

export default function Home() {
  return (
    <div>
      <h1 className="text-nanobank-blue-deep font-bold">Nano-Bank</h1>
      <Link className="text-nanobank-blue-green" href="/health">API Health Check</Link>
    </div>
  );
}
